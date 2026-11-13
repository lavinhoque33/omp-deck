//! Worktree creation stays off the gateway loop and uses the shared creation flow.
use super::*;
use deck_core::worktree;
use std::sync::Mutex;

// Choosing the default branch and creating it is one operation, including parallel tool calls.
static CREATE: Mutex<()> = Mutex::new(());

#[derive(Deserialize)]
pub(super) struct OpenWorktree {
    branch: Option<String>,
    base: Option<String>,
}

impl Engine {
    pub(super) fn begin_agent_worktree_open(
        &self,
        request: u64,
        event: ServerEvent,
        args: OpenWorktree,
    ) {
        let ServerEvent::AgentOpenSession { cwd, .. } = &event else {
            unreachable!("open event");
        };
        let cwd = cwd.clone();
        let omp = self.omp.clone();
        let settings = self.settings.clone();
        self.spawn_worker("agent-worktree", move || {
            let result = (|| {
                let _creation = CREATE
                    .lock()
                    .map_err(|_| "Worktree creation lock failed".to_owned())?;
                let info = worktree::repo_info(&cwd).map_err(|e| e.to_string())?;
                let primary = if info.root == info.primary {
                    info
                } else {
                    worktree::repo_info(&info.primary).map_err(|e| e.to_string())?
                };
                let branch = args.branch.unwrap_or_else(|| {
                    unique_branch(
                        &branch_stamp(crate::gateway::unix_ms() / 1000),
                        &primary.branches,
                    )
                });
                let base = args
                    .base
                    .filter(|base| !base.trim().is_empty())
                    .or(primary.current_branch)
                    .unwrap_or_else(|| "HEAD".to_owned());
                let path = super::super::worktree::create_new(
                    omp.as_ref(),
                    &settings,
                    &primary.primary,
                    &branch,
                    Some(base.clone()),
                    None,
                )?;
                Ok((path, branch.trim().to_owned(), base))
            })();
            Fin::AgentWorktreeOpen {
                request,
                event: Box::new(event),
                result,
            }
        });
    }

    pub(in crate::gateway::engine) fn agent_worktree_opened(
        &mut self,
        request: u64,
        mut event: ServerEvent,
        result: Result<(PathBuf, String, String), String>,
    ) {
        if !self.agent_tool_current(request) {
            return;
        }
        let (path, branch, base) = match result {
            Ok(created) => created,
            Err(error) => return self.finish_agent_tool(request, false, error),
        };
        let ServerEvent::AgentOpenSession { cwd, .. } = &mut event else {
            unreachable!("open event");
        };
        *cwd = path.clone();
        self.agent_tools
            .pending
            .get_mut(&request)
            .expect("current request")
            .worktree_result = Some(format!(
            "Worktree: {} on branch {branch} (merges into {base})",
            path.display()
        ));
        if self.to_desktop(&event) {
            self.agent_tools
                .pending
                .get_mut(&request)
                .expect("current request")
                .deadline = Some(Instant::now() + Duration::from_secs(30));
        } else {
            self.finish_agent_tool(request, false, "OmpDeck's window is not open".to_owned());
        }
    }
}

fn unique_branch(stamp: &str, branches: &[String]) -> String {
    let mut branch = stamp.to_owned();
    let mut suffix = 2;
    while branches.contains(&branch) {
        branch = format!("{stamp}-{suffix}");
        suffix += 1;
    }
    branch
}

// UTC, matching the existing worktree dialog suggestion (Hinnant civil-date conversion).
fn branch_stamp(unix_secs: u64) -> String {
    let days = (unix_secs / 86_400) as i64;
    let rem = unix_secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "wt/{year:04}{month:02}{day:02}-{:02}{:02}",
        rem / 3_600,
        rem % 3_600 / 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_suggestion_is_utc_and_avoids_existing_names() {
        assert_eq!(branch_stamp(0), "wt/19700101-0000");
        assert_eq!(branch_stamp(1_792_750_079), "wt/20261023-1007");
        let stamp = "wt/20261112-1007";
        assert_eq!(unique_branch(stamp, &[]), stamp);
        assert_eq!(
            unique_branch(stamp, &[stamp.into(), format!("{stamp}-2")]),
            format!("{stamp}-3")
        );
        assert_eq!(
            unique_branch(stamp, &[stamp.into(), format!("{stamp}-3")]),
            format!("{stamp}-2")
        );
    }
}
