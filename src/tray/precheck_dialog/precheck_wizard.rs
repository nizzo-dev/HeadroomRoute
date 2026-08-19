use crate::precheck::{PrecheckAction, PrecheckItem, PrecheckReport, PrecheckStatus};

pub(super) fn first_actionable(report: &PrecheckReport) -> Option<&PrecheckItem> {
    report.items.iter().find(|item| {
        matches!(item.status, PrecheckStatus::Fail | PrecheckStatus::Warning)
            && item.action.is_some()
    })
}

pub(super) fn wizard_actions(report: &PrecheckReport) -> Vec<PrecheckAction> {
    report.actions().into_iter().take(1).collect()
}

pub(super) fn wizard_text(report: &PrecheckReport) -> String {
    let remaining = report
        .items
        .iter()
        .filter(|item| {
            matches!(item.status, PrecheckStatus::Fail | PrecheckStatus::Warning)
                && item.action.is_some()
        })
        .count();
    if let Some(item) = first_actionable(report) {
        return format!(
            "需要处理（还剩 {remaining} 项）\r\n\r\n[{}] {}\r\n说明：{}\r\n建议：{}\r\n\r\n点击下方按钮处理后，再点「重新检测」。完整报告可点「复制报告」。",
            item.status.label(),
            item.name,
            item.description,
            item.advice
        );
    }
    let notes = report
        .items
        .iter()
        .filter(|item| item.status == PrecheckStatus::Warning)
        .map(|item| {
            format!(
                "[{}] {}\r\n说明：{}\r\n建议：{}",
                item.status.label(),
                item.name,
                item.description,
                item.advice
            )
        })
        .collect::<Vec<_>>();
    if notes.is_empty() {
        format!(
            "预检通过。\r\n\r\n{}\r\n\r\nCodex、Claude 与 Headroom 当前可用。完整报告可点「复制报告」。",
            report.summary_line()
        )
    } else {
        format!(
            "预检通过，有提示。\r\n\r\n{}\r\n\r\n{}\r\n\r\n这些提示无需立即操作。完整报告可点「复制报告」。",
            report.summary_line(),
            notes.join("\r\n\r\n")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::precheck::{PrecheckFacts, evaluate};

    fn pass_facts() -> PrecheckFacts {
        PrecheckFacts {
            mode_needs_headroom: true,
            python_found: true,
            codex_enabled: true,
            claude_enabled: true,
            direct_codex: false,
            direct_claude: false,
            bypass_headroom: false,
            codex_has_route: true,
            claude_has_route: true,
            codex_config_exists: true,
            claude_settings_exists: true,
            cc_switch_db_exists: true,
            agent_port: 8790,
            headroom_port: 8787,
        }
    }

    #[test]
    fn passing_report_has_no_wizard_action() {
        let report = evaluate(&pass_facts(), "advice");
        assert!(first_actionable(&report).is_none());
        assert!(wizard_actions(&report).is_empty());
        assert!(wizard_text(&report).contains("预检通过"));
    }

    #[test]
    fn missing_runtime_offers_select_python_first() {
        let mut facts = pass_facts();
        facts.python_found = false;
        let report = evaluate(&facts, "请选择 Python");
        let item = first_actionable(&report).expect("expected Headroom failure");
        assert_eq!(item.name, "Headroom 运行环境");
        assert_eq!(wizard_actions(&report), vec![PrecheckAction::SelectPython]);
        assert!(wizard_text(&report).contains("需要处理"));
        assert!(wizard_text(&report).contains("请选择 Python"));
    }

    #[test]
    fn first_actionable_is_the_first_fail_with_a_button() {
        let mut facts = pass_facts();
        facts.python_found = false;
        facts.codex_has_route = false;
        let report = evaluate(&facts, "请选择 Python");
        let item = first_actionable(&report).unwrap();
        assert_eq!(item.name, "Headroom 运行环境");
        assert_eq!(wizard_actions(&report), vec![PrecheckAction::SelectPython]);
    }
}
