//! One-shot, opener-root-fenced child reports. Only an acknowledged delivery is success.
use super::*;
use deck_core::ctl::CtlCommand;

const NO_OPENER: &str = "No session opened this one; nothing to report to";
const OPENER_ENDED: &str = "The session that opened this one has ended or moved on";
const RATE_LIMITED: &str = "Too many reports; send one when something changes";
const INTERVAL: Duration = Duration::from_secs(10);
const BURST: Duration = Duration::from_secs(30);

pub(super) struct ReportRate {
    credit: Duration,
    at: Instant,
    reserved: u32,
}
impl ReportRate {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            credit: BURST,
            at: now,
            reserved: 0,
        }
    }
    fn refill(&mut self, now: Instant) {
        self.credit = self
            .credit
            .saturating_add(now.saturating_duration_since(self.at))
            .min(BURST);
        self.at = now;
    }
    fn reserve(&mut self, now: Instant) -> bool {
        self.refill(now);
        if self.credit < INTERVAL.saturating_mul(self.reserved + 1) {
            return false;
        }
        self.reserved += 1;
        true
    }
    fn commit(&mut self, now: Instant) {
        self.refill(now);
        self.reserved -= 1;
        self.credit -= INTERVAL;
    }
    fn release(&mut self) {
        self.reserved -= 1;
    }
    #[cfg(test)]
    fn take(&mut self, now: Instant) -> bool {
        if !self.reserve(now) {
            return false;
        }
        self.commit(now);
        true
    }
    fn refund(&mut self) {
        self.credit = self.credit.saturating_add(INTERVAL).min(BURST);
    }
}

#[derive(Deserialize)]
struct ReportArgs {
    text: String,
    #[serde(default)]
    kind: ReportKind,
}
#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ReportKind {
    #[default]
    Progress,
    Blocked,
    Done,
}
impl ReportArgs {
    fn message(self) -> Result<String, String> {
        if self.text.is_empty() || self.text.len() > 4096 {
            return Err("text must contain 1 to 4096 bytes".to_owned());
        }
        Ok(match self.kind {
            ReportKind::Progress => self.text,
            ReportKind::Blocked => format!("[blocked] {}", self.text),
            ReportKind::Done => format!("[done] {}", self.text),
        })
    }
}

pub(super) struct ReportDelivery {
    handle: String,
    text: String,
    report: crate::gateway::protocol::OrchReport,
    sent: Option<(SessionTarget, u64, String)>,
    until: Instant,
}
impl ReportDelivery {
    pub(super) fn needs_controller(&self, key: &SessionKey, registry: &Registry) -> bool {
        registry
            .entries
            .get(&self.handle)
            .is_some_and(|child| &child.opener.key == key)
    }
}

impl Engine {
    pub(in crate::gateway::engine) fn begin_child_report(
        &mut self,
        request: u64,
        args: serde_json::Value,
        now: Instant,
    ) -> Result<(), String> {
        let args: ReportArgs = serde_json::from_value(args).map_err(|e| e.to_string())?;
        let report = crate::gateway::protocol::OrchReport {
            kind: match args.kind {
                ReportKind::Progress => "progress",
                ReportKind::Blocked => "blocked",
                ReportKind::Done => "done",
            }
            .to_owned(),
            text: args.text.clone(),
            at_unix_ms: crate::gateway::unix_ms(),
        };
        let text = args.message()?;
        let pending = &self.agent_tools.pending[&request];
        let index = self
            .sess
            .find(&pending.target.key)
            .ok_or_else(|| NO_OPENER.to_owned())?;
        let handle = self
            .agent_tools
            .children
            .entries
            .iter()
            .find_map(|(handle, child)| {
                (child.key == pending.target.key && !child.invalidated).then(|| handle.clone())
            })
            .ok_or_else(|| NO_OPENER.to_owned())?;
        self.validate_child_identity(&handle, index)?;
        let child = &self.agent_tools.children.entries[&handle];
        if self.background_opener_ended(child) {
            return Err(OPENER_ENDED.to_owned());
        }
        let child = self
            .agent_tools
            .children
            .entries
            .get_mut(&handle)
            .expect("registered child");
        if !child.reports.reserve(now) {
            return Err(RATE_LIMITED.to_owned());
        }
        self.agent_tools.children.reports.insert(
            request,
            ReportDelivery {
                handle,
                text,
                report,
                sent: None,
                until: now + DELIVERY,
            },
        );
        self.waker.now();
        Ok(())
    }

    fn finish_child_report(&mut self, request: u64, ok: bool, text: String, refund: bool) {
        let opener = self
            .agent_tools
            .children
            .reports
            .remove(&request)
            .and_then(|report| {
                let child = self.agent_tools.children.entries.get_mut(&report.handle)?;
                if refund {
                    if report.sent.is_some() {
                        child.reports.refund();
                    } else {
                        child.reports.release();
                    }
                }
                Some(child.opener.key.clone())
            });
        self.finish_agent_tool(request, ok, text);
        if let Some(opener) = opener {
            self.release_idle_child_attachment(&opener);
        }
    }

    pub(super) fn step_child_reports(&mut self, now: Instant) {
        let requests: Vec<_> = self.agent_tools.children.reports.keys().copied().collect();
        for request in requests {
            let report = &self.agent_tools.children.reports[&request];
            if !self.agent_tools.pending.contains_key(&request) {
                self.finish_child_report(request, false, String::new(), report.sent.is_none());
                continue;
            }
            let child = &self.agent_tools.children.entries[&report.handle];
            if self.background_opener_ended(child) {
                self.finish_child_report(
                    request,
                    false,
                    OPENER_ENDED.to_owned(),
                    report.sent.is_none(),
                );
                continue;
            }
            if now >= report.until {
                self.finish_child_report(
                    request,
                    false,
                    "The report delivery could not be confirmed".to_owned(),
                    report.sent.is_none(),
                );
                continue;
            }
            if let Some((target, rid, _)) = &report.sent {
                if !self
                    .agent_tools
                    .reply_acks
                    .contains(&(target.clone(), *rid))
                {
                    self.finish_child_report(
                        request,
                        false,
                        "The report delivery could not be confirmed".to_owned(),
                        false,
                    );
                }
                continue;
            }
            let Some(index) = self.background_delivery_target(&child.opener_root) else {
                continue;
            };
            let session = &self.sess.managed[index];
            let title = self
                .sess
                .find(&child.key)
                .map(|i| self.sess.managed[i].display_label())
                .unwrap_or_else(|| child.attach.label.clone());
            let command = CtlCommand::Deliver {
                root: child.opener_root.clone(),
                title: format!("Report from {title} ({})", report.handle),
                text: report.text.clone(),
                details: Some(serde_json::json!({"handle": report.handle, "report": true})),
            };
            let target = session.ctl.target().clone();
            let opener_title = session.display_label();
            let rid = session
                .live
                .as_ref()
                .expect("eligible live link")
                .client
                .send(command);
            self.agent_tools
                .children
                .entries
                .get_mut(&report.handle)
                .expect("registered child")
                .reports
                .commit(now);
            self.agent_tools.reply_acks.push((target.clone(), rid));
            self.agent_tools
                .children
                .reports
                .get_mut(&request)
                .expect("pending report")
                .sent = Some((target, rid, opener_title));
        }
        if !self.agent_tools.children.reports.is_empty() {
            self.wake_within(POLL);
        }
    }

    pub(super) fn child_report_reply_done(
        &mut self,
        key: &SessionKey,
        rid: u64,
        ok: bool,
        root_mismatch: bool,
        error: Option<String>,
    ) {
        let request = self
            .agent_tools
            .children
            .reports
            .iter()
            .find_map(|(&request, report)| {
                report
                    .sent
                    .as_ref()
                    .is_some_and(|(target, pending, _)| &target.key == key && *pending == rid)
                    .then_some(request)
            });
        let Some(request) = request else {
            return;
        };
        let report = &self.agent_tools.children.reports[&request];
        let child = &self.agent_tools.children.entries[&report.handle];
        if root_mismatch || self.background_opener_ended(child) {
            self.finish_child_report(request, false, OPENER_ENDED.to_owned(), !ok);
        } else if ok {
            let title = &report.sent.as_ref().expect("sent report").2;
            let result = format!("Reported to {title}.");
            let handle = report.handle.clone();
            let report = report.report.clone();
            self.agent_tools
                .children
                .entries
                .get_mut(&handle)
                .expect("registered child")
                .last_report = Some(report);
            self.finish_child_report(request, true, result, false);
        } else {
            self.finish_child_report(
                request,
                false,
                format!(
                    "The report was not delivered: {}",
                    error.unwrap_or_else(|| "the opener refused it".to_owned())
                ),
                true,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reports_refill_one_per_ten_seconds_with_burst_three() {
        let now = Instant::now();
        let mut rate = ReportRate::new(now);
        for _ in 0..3 {
            assert!(rate.take(now));
        }
        assert!(!rate.take(now));
        assert!(!rate.take(now + Duration::from_secs(9)));
        assert!(rate.take(now + INTERVAL));
        assert!(!rate.take(now + INTERVAL));
        for _ in 0..3 {
            assert!(rate.take(now + Duration::from_secs(100)));
        }
        assert!(!rate.take(now + Duration::from_secs(100)));
        rate.refund();
        assert!(rate.take(now + Duration::from_secs(100)));
    }
    #[test]
    fn held_reports_cannot_accumulate_a_later_delivery_burst() {
        let now = Instant::now();
        let mut rate = ReportRate::new(now);
        for _ in 0..3 {
            assert!(rate.reserve(now));
        }
        assert!(!rate.reserve(now + Duration::from_secs(20)));
        for _ in 0..3 {
            rate.commit(now + Duration::from_secs(20));
        }
        assert!(!rate.reserve(now + Duration::from_secs(20)));
        assert!(rate.reserve(now + Duration::from_secs(30)));
        rate.release();
        assert!(rate.reserve(now + Duration::from_secs(30)));
    }
    #[test]
    fn report_prefixes_and_byte_limits_are_exact() {
        let message = |text: &str, kind: ReportKind| {
            ReportArgs {
                text: text.into(),
                kind,
            }
            .message()
        };
        assert_eq!(message("hello", ReportKind::Progress).unwrap(), "hello");
        assert_eq!(
            message("hello", ReportKind::Blocked).unwrap(),
            "[blocked] hello"
        );
        assert_eq!(message("hello", ReportKind::Done).unwrap(), "[done] hello");
        assert!(message("", ReportKind::Progress).is_err());
        assert!(message(&"é".repeat(2048), ReportKind::Progress).is_ok());
        assert!(message(&"é".repeat(2049), ReportKind::Progress).is_err());
    }
}
