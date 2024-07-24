use core::time;
use std::{
    collections::BTreeSet,
    fmt::Display,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use anyhow::Context;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::{
    goal::{self, GoalDocument, ParsedOwners, PlanItem, Status},
    team::{get_person_data, TeamName},
};

fn validate_path(path: &Path) -> anyhow::Result<String> {
    if !path.is_dir() {
        return Err(anyhow::anyhow!(
            "RFC path should be a directory like src/2024h2"
        ));
    };

    if path.is_absolute() {
        return Err(anyhow::anyhow!("RFC path should be relative"));
    }

    let timeframe = path
        .components()
        .last()
        .unwrap()
        .as_os_str()
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("invalid path `{}`", path.display()))?;

    Ok(timeframe.to_string())
}

pub fn generate_comment(path: &Path) -> anyhow::Result<()> {
    let _ = validate_path(path)?;
    let goal_documents = goal::goals_in_dir(path)?;
    let teams_with_asks = teams_with_asks(&goal_documents);

    for team_name in teams_with_asks {
        let team_data = team_name.data();

        println!("\n## {}\n", team_data.name);

        let (leads, members): (Vec<_>, Vec<_>) = team_data.members.iter().partition(|m| m.is_lead);

        for lead in leads {
            println!("* [ ] @{} (required, lead)", lead.github);
        }

        for member in members {
            println!("* [ ] @{} (optional)", member.github);
        }
    }

    Ok(())
}

pub fn generate_rfc(path: &Path) -> anyhow::Result<()> {
    let timeframe = &validate_path(path)?;

    // run mdbook build
    Command::new("mdbook").arg("build").status()?;

    // find the markdown output
    let generated_path = PathBuf::from("book/markdown")
        .join(timeframe)
        .join("index.md");
    if !generated_path.exists() {
        return Err(anyhow::anyhow!(
            "no markdown generated at {}",
            generated_path.display()
        ));
    }

    let generated_text = std::fs::read_to_string(&generated_path).with_context(|| {
        format!(
            "reading generated markdown from `{}`",
            generated_path.display()
        )
    })?;

    let regex = Regex::new(r"\((.*).md(#[^)]*)?\)").unwrap();

    let result = regex.replace_all(
        &generated_text,
        format!("(https://rust-lang.github.io/rust-project-goals/{timeframe}/$1.html$2)"),
    );

    println!("{result}");

    Ok(())
}

pub fn generate_issues(
    repository: &str,
    path: &Path,
    commit: bool,
    sleep: u64,
) -> anyhow::Result<()> {
    let timeframe = validate_path(path)?;

    let mut goal_documents = goal::goals_in_dir(path)?;
    goal_documents.retain(|gd| gd.is_not_not_accepted());

    let teams_with_asks = teams_with_asks(&goal_documents);
    let mut actions = initialize_labels(repository, &teams_with_asks)?;
    actions.extend(initialize_issues(repository, &timeframe, &goal_documents)?);

    if actions.is_empty() {
        eprintln!("No actions to be executed.");
        return Ok(());
    }

    if commit {
        progress_bar::init_progress_bar(actions.len());
        progress_bar::set_progress_bar_action(
            "Executing",
            progress_bar::Color::Blue,
            progress_bar::Style::Bold,
        );
        for action in actions.into_iter() {
            progress_bar::print_progress_bar_info(
                "Action",
                &format!("{}", action),
                progress_bar::Color::Green,
                progress_bar::Style::Bold,
            );
            action.execute(repository, &timeframe)?;
            progress_bar::inc_progress_bar();

            std::thread::sleep(Duration::from_millis(sleep));
        }
        progress_bar::finalize_progress_bar();
    } else {
        eprintln!("Actions to be executed:");
        for action in &actions {
            eprintln!("* {action}");
        }
        eprintln!("");
        eprintln!("Use `--commit` to execute the actions.");
    }

    Ok(())
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct GithubIssue {
    pub title: String,
    pub assignees: Vec<String>,
    pub body: String,
    pub labels: Vec<String>,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum GithubAction {
    CreateLabel { label: GhLabel },
    CreateIssue { issue: GithubIssue },
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
struct GhLabel {
    name: String,
    color: String,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
struct ExistingGithubIssue {
    title: String,
}

fn list_labels(repository: &str) -> anyhow::Result<Vec<GhLabel>> {
    let output = Command::new("gh")
        .arg("-R")
        .arg(repository)
        .arg("label")
        .arg("list")
        .arg("--json")
        .arg("name,color")
        .output()?;

    let labels: Vec<GhLabel> = serde_json::from_slice(&output.stdout)?;

    Ok(labels)
}

fn list_issue_titles_in_milestone(
    repository: &str,
    timeframe: &str,
) -> anyhow::Result<BTreeSet<String>> {
    let output = Command::new("gh")
        .arg("-R")
        .arg(repository)
        .arg("issue")
        .arg("list")
        .arg("-m")
        .arg(timeframe)
        .arg("--json")
        .arg("title")
        .output()?;

    let existing_issues: Vec<ExistingGithubIssue> = serde_json::from_slice(&output.stdout)?;

    Ok(existing_issues.into_iter().map(|e_i| e_i.title).collect())
}
/// Initializes the required `T-<team>` labels on the repository.
/// Warns if the labels are found with wrong color.
fn initialize_labels(
    repository: &str,
    teams_with_asks: &BTreeSet<&TeamName>,
) -> anyhow::Result<BTreeSet<GithubAction>> {
    const TEAM_LABEL_COLOR: &str = "bfd4f2";

    let mut desired_labels: BTreeSet<_> = teams_with_asks
        .iter()
        .map(|team| {
            let label_name = team.gh_label();

            GhLabel {
                name: label_name,
                color: TEAM_LABEL_COLOR.to_string(),
            }
        })
        .collect();

    desired_labels.insert(GhLabel {
        name: "C-tracking-issue".to_string(),
        color: "f5f1fd".to_string(),
    });

    desired_labels.insert(GhLabel {
        name: "Flagship Goal".to_string(),
        color: "5319E7".to_string(),
    });

    for existing_label in list_labels(repository)? {
        desired_labels.remove(&existing_label);
    }

    Ok(desired_labels
        .into_iter()
        .map(|label| GithubAction::CreateLabel { label })
        .collect())
}

/// Initializes the required `T-<team>` labels on the repository.
/// Warns if the labels are found with wrong color.
fn initialize_issues(
    repository: &str,
    timeframe: &str,
    goal_documents: &[GoalDocument],
) -> anyhow::Result<BTreeSet<GithubAction>> {
    // the set of issues we want to exist
    let mut desired_issues: BTreeSet<GithubIssue> = goal_documents
        .iter()
        .map(|goal_document| issue(timeframe, goal_document))
        .collect::<anyhow::Result<_>>()?;

    // remove any existings that already exist
    let existing_issues = list_issue_titles_in_milestone(repository, timeframe)?;
    desired_issues.retain(|i| !existing_issues.contains(&i.title));

    Ok(desired_issues
        .into_iter()
        .map(|issue| GithubAction::CreateIssue { issue })
        .collect())
}

fn issue(timeframe: &str, document: &GoalDocument) -> anyhow::Result<GithubIssue> {
    let mut assignees = vec![];
    for username in document.metadata.owner_usernames() {
        if get_person_data(username)?.is_some() {
            assignees.push(username[1..].to_string());
        }
    }

    let mut labels = vec!["C-tracking-issue".to_string()];
    if let Status::Flagship = document.metadata.status {
        labels.push("Flagship Goal".to_string());
    }
    for team in document.teams_with_asks() {
        labels.push(team.gh_label());
    }

    Ok(GithubIssue {
        title: document.metadata.title.clone(),
        assignees,
        body: issue_text(timeframe, document)?,
        labels,
    })
}

fn issue_text(timeframe: &str, document: &GoalDocument) -> anyhow::Result<String> {
    let mut tasks = vec![];
    for plan_item in &document.plan_items {
        tasks.extend(task_items(plan_item)?);
    }

    let teams = document
        .teams_with_asks()
        .iter()
        .map(|team| team.name_and_link())
        .collect::<Vec<_>>();

    let goal_file = document.link_path.file_stem().unwrap().to_str().unwrap();

    Ok(format!(
        r##"
| Metadata      | |
| --------      | --- |
| Owner(s)      | {owners} |
| Team(s)       | {teams} |
| Goal document | [{timeframe}/{goal_file}](https://rust-lang.github.io/rust-project-goals/{timeframe}/{goal_file}.html) |

## Summary

{summary}

## Tasks and status

{tasks}

[Team]: https://img.shields.io/badge/Team%20ask-red
"##,
        owners = &document.metadata.owner_usernames().join(", "),
        teams = teams.join(", "),
        summary = document.summary,
        tasks = tasks.join("\n"),
    ))
}

fn task_items(plan_item: &PlanItem) -> anyhow::Result<Vec<String>> {
    use std::fmt::Write;

    let mut tasks = vec![];

    let mut description = format!(
        "* {box} {text}",
        box = if plan_item.is_complete() { "[x]" } else { "[ ]" },
        text = plan_item.text
    );

    if let Some(parsed_owners) = plan_item.parse_owners()? {
        match parsed_owners {
            ParsedOwners::TeamAsks(asks) => {
                let teams: Vec<String> = asks.iter().map(|ask| ask.name_and_link()).collect();

                write!(description, " ({} ![Team][])", teams.join(", "))?;
            }

            ParsedOwners::Usernames(usernames) => {
                write!(description, " ({})", usernames.join(", "))?;
            }
        }
    }

    tasks.push(description);

    for task in &plan_item.children {
        tasks.extend(task_items(task)?.into_iter().map(|t| format!("  {}", &t)));
    }

    Ok(tasks)
}

fn teams_with_asks(goal_documents: &[GoalDocument]) -> BTreeSet<&'static TeamName> {
    goal_documents
        .iter()
        .flat_map(|g| &g.team_asks)
        .flat_map(|ask| &ask.teams)
        .copied()
        .collect()
}

impl Display for GithubAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GithubAction::CreateLabel {
                label: GhLabel { name, color },
            } => {
                write!(f, "create label `{}` with color `{}`", name, color)
            }
            GithubAction::CreateIssue { issue } => {
                write!(f, "create issue \"{}\"", issue.title)
            }
        }
    }
}

impl GithubAction {
    pub fn execute(self, repository: &str, timeframe: &str) -> anyhow::Result<()> {
        match self {
            GithubAction::CreateLabel {
                label: GhLabel { name, color },
            } => {
                let output = Command::new("gh")
                    .arg("-R")
                    .arg(repository)
                    .arg("label")
                    .arg("create")
                    .arg(&name)
                    .arg("--color")
                    .arg(&color)
                    .arg("--force")
                    .output()?;

                if !output.status.success() {
                    Err(anyhow::anyhow!(
                        "failed to create label `{}`: {}",
                        name,
                        String::from_utf8_lossy(&output.stderr)
                    ))
                } else {
                    Ok(())
                }
            }

            GithubAction::CreateIssue {
                issue:
                    GithubIssue {
                        title,
                        assignees,
                        body,
                        labels,
                    },
            } => {
                let output = Command::new("gh")
                    .arg("-R")
                    .arg(&repository)
                    .arg("issue")
                    .arg("create")
                    .arg("-b")
                    .arg(&body)
                    .arg("-t")
                    .arg(&title)
                    .arg("-l")
                    .arg(labels.join(","))
                    .arg("-a")
                    .arg(assignees.join(","))
                    .arg("-m")
                    .arg(&timeframe)
                    .output()?;

                if !output.status.success() {
                    Err(anyhow::anyhow!(
                        "failed to create issue `{}`: {}",
                        title,
                        String::from_utf8_lossy(&output.stderr)
                    ))
                } else {
                    Ok(())
                }
            }
        }
    }
}
