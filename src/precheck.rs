//! 只读预检：把本地状态收集与纯判断分离，产出可操作的中文 doctor 报告。
//!
//! 收集阶段只读取本地配置与文件，或启动只读的 Python 版本验证；不联网、不发起
//! 健康请求、不写配置、不读取或输出 API Key。判断阶段 [`evaluate`] 完全确定，
//! 便于在没有真实用户配置、网络或 Headroom 安装的环境中做单元测试。

use crate::{
    config,
    model::{
        AppConfig, Protocol, RouteHealth, RuntimeStatus, RuntimeStatusInput,
        evaluate_runtime_status,
    },
    runtime,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrecheckStatus {
    Pass,
    Warning,
    Fail,
    Skip,
}

impl PrecheckStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Pass => "通过",
            Self::Warning => "警告",
            Self::Fail => "失败",
            Self::Skip => "跳过",
        }
    }
}

/// 预检报告可执行的修复动作。由报告按强类型推导并去重，界面据此直接呈现，
/// 不依赖解析任何中文文案。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum PrecheckAction {
    /// Headroom 运行环境缺失：打开 Python 文件选择。
    SelectPython,
    /// Codex/Claude 路由缺失或启用客户端配置缺失：同步 Codex + Claude / CC-Switch。
    SyncRoutes,
    /// 端口冲突：打开 config.json 手工调整。
    OpenConfig,
}

#[derive(Clone, Debug)]
pub struct PrecheckItem {
    pub name: &'static str,
    pub status: PrecheckStatus,
    pub description: String,
    pub advice: String,
    pub action: Option<PrecheckAction>,
}

impl PrecheckItem {
    fn pass(name: &'static str, description: impl Into<String>) -> Self {
        Self {
            name,
            status: PrecheckStatus::Pass,
            description: description.into(),
            advice: "无需操作".into(),
            action: None,
        }
    }

    fn warning(
        name: &'static str,
        description: impl Into<String>,
        advice: impl Into<String>,
        action: Option<PrecheckAction>,
    ) -> Self {
        Self {
            name,
            status: PrecheckStatus::Warning,
            description: description.into(),
            advice: advice.into(),
            action,
        }
    }

    fn fail(
        name: &'static str,
        description: impl Into<String>,
        advice: impl Into<String>,
        action: Option<PrecheckAction>,
    ) -> Self {
        Self {
            name,
            status: PrecheckStatus::Fail,
            description: description.into(),
            advice: advice.into(),
            action,
        }
    }

    fn skip(name: &'static str, description: impl Into<String>) -> Self {
        Self {
            name,
            status: PrecheckStatus::Skip,
            description: description.into(),
            advice: "无需操作".into(),
            action: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PrecheckReport {
    pub items: Vec<PrecheckItem>,
    pub runtime_status: RuntimeStatus,
}

impl PrecheckReport {
    pub fn count(&self, status: PrecheckStatus) -> usize {
        self.items
            .iter()
            .filter(|item| item.status == status)
            .count()
    }

    pub fn summary_line(&self) -> String {
        format!(
            "预检结果：通过 {}，警告 {}，失败 {}，跳过 {}",
            self.count(PrecheckStatus::Pass),
            self.count(PrecheckStatus::Warning),
            self.count(PrecheckStatus::Fail),
            self.count(PrecheckStatus::Skip),
        )
    }

    /// 当前报告实际需要且去重后的修复动作。仅失败与警告项可能携带动作，
    /// 同类问题（例如 Codex 与 Claude 同时缺路由或配置文件）只出现一次。
    pub fn actions(&self) -> Vec<PrecheckAction> {
        let mut actions = Vec::new();
        for item in &self.items {
            if matches!(item.status, PrecheckStatus::Fail | PrecheckStatus::Warning)
                && let Some(action) = item.action
                && !actions.contains(&action)
            {
                actions.push(action);
            }
        }
        actions
    }

    pub fn to_text(&self) -> String {
        let mut lines = vec![
            format!("运行结论：{}", self.runtime_status.summary()),
            format!("Codex：{}", self.runtime_status.codex.summary()),
            format!("Claude：{}", self.runtime_status.claude.summary()),
            format!("Headroom：{}", self.runtime_status.headroom.summary()),
            String::new(),
            self.summary_line(),
            String::new(),
        ];
        for item in &self.items {
            lines.push(format!("[{}] {}", item.status.label(), item.name));
            lines.push(format!("说明：{}", item.description));
            lines.push(format!("建议：{}", item.advice));
            lines.push(String::new());
        }
        while matches!(lines.last(), Some(line) if line.is_empty()) {
            lines.pop();
        }
        lines.join("\r\n")
    }
}

/// 环境相关事实。收集阶段负责填满，判断阶段只依赖这些值。
#[derive(Clone, Debug)]
pub struct PrecheckFacts {
    pub mode_needs_headroom: bool,
    pub python_found: bool,
    pub codex_enabled: bool,
    pub claude_enabled: bool,
    pub direct_codex: bool,
    pub direct_claude: bool,
    pub bypass_headroom: bool,
    pub codex_has_route: bool,
    pub claude_has_route: bool,
    pub codex_config_exists: bool,
    pub claude_settings_exists: bool,
    pub cc_switch_db_exists: bool,
    pub agent_port: u16,
    pub headroom_port: u16,
}

/// 观测模式 / 旁路 / 协议全禁用时不需要 Headroom；接管上游且至少启用一个协议时需要。
pub fn mode_needs_headroom(config: &AppConfig) -> bool {
    if config.bypass_headroom || !config.manage_upstream {
        return false;
    }
    config.enable_codex || config.enable_claude
}

fn collect_facts(config: &AppConfig, python_found: bool) -> PrecheckFacts {
    let discovered = config::discover_routes(config).ok();
    let has_route = |protocol: Protocol| {
        discovered
            .as_ref()
            .is_some_and(|found| found.routes.iter().any(|route| route.protocol == protocol))
    };
    PrecheckFacts {
        mode_needs_headroom: mode_needs_headroom(config),
        python_found,
        codex_enabled: config.enable_codex,
        claude_enabled: config.enable_claude,
        direct_codex: config.direct_codex,
        direct_claude: config.direct_claude,
        bypass_headroom: config.bypass_headroom,
        codex_has_route: has_route(Protocol::OpenAi),
        claude_has_route: has_route(Protocol::Anthropic),
        codex_config_exists: config.codex_config.exists(),
        claude_settings_exists: config.claude_settings.exists(),
        cc_switch_db_exists: config.cc_switch_db.exists(),
        agent_port: config.agent_port,
        headroom_port: config.headroom_port,
    }
}

/// 真实环境收集：读取本地状态；仅在需要 Headroom 时启动只读 Python 版本验证。
pub fn collect(config: &AppConfig) -> PrecheckReport {
    let python_found = mode_needs_headroom(config) && runtime::find_valid_python(config).is_some();
    collect_with(config, python_found)
}

/// 注入 Python 探测结果的收集，便于测试脱离真实 Headroom 安装。
pub fn collect_with(config: &AppConfig, python_found: bool) -> PrecheckReport {
    let advice = runtime::setup_instructions(config);
    evaluate(&collect_facts(config, python_found), &advice)
}

/// 纯判断：根据事实确定每个预检项的通过、警告、失败或跳过。
pub fn evaluate(facts: &PrecheckFacts, headroom_setup_advice: &str) -> PrecheckReport {
    let mut items = Vec::with_capacity(7);

    if !facts.mode_needs_headroom {
        items.push(PrecheckItem::skip(
            "Headroom 运行环境",
            "当前为旁路或双直连模式，不经过 Headroom，无需本地运行环境",
        ));
    } else if facts.python_found {
        items.push(PrecheckItem::pass(
            "Headroom 运行环境",
            format!(
                "当前模式需要 Headroom，已找到可用的 Python 与 Headroom {}",
                runtime::HEADROOM_VERSION
            ),
        ));
    } else {
        items.push(PrecheckItem::fail(
            "Headroom 运行环境",
            format!(
                "当前模式需要 Headroom，但未找到可用的 Python 与 Headroom {}",
                runtime::HEADROOM_VERSION
            ),
            headroom_setup_advice,
            Some(PrecheckAction::SelectPython),
        ));
    }

    if !facts.codex_enabled {
        items.push(PrecheckItem::skip("Codex 路由", "Codex 未启用"));
    } else if facts.codex_has_route {
        items.push(PrecheckItem::pass("Codex 路由", "已发现可用的 OpenAI 路由"));
    } else {
        items.push(PrecheckItem::fail(
            "Codex 路由",
            "Codex 已启用，但未发现可用的 OpenAI 路由",
            "请在 CC-Switch 添加 Codex Provider，或从托盘选择“同步 Codex + Claude / CC-Switch”",
            Some(PrecheckAction::SyncRoutes),
        ));
    }

    if !facts.claude_enabled {
        items.push(PrecheckItem::skip("Claude 路由", "Claude 未启用"));
    } else if facts.claude_has_route {
        items.push(PrecheckItem::pass(
            "Claude 路由",
            "已发现可用的 Anthropic 路由",
        ));
    } else {
        items.push(PrecheckItem::fail(
            "Claude 路由",
            "Claude 已启用，但未发现可用的 Anthropic 路由",
            "请在 CC-Switch 添加 Claude Provider，或从托盘选择“同步 Codex + Claude / CC-Switch”",
            Some(PrecheckAction::SyncRoutes),
        ));
    }

    if facts.cc_switch_db_exists {
        items.push(PrecheckItem::pass(
            "CC-Switch 数据库",
            "可选的 CC-Switch 数据库存在，可同步 Provider",
        ));
    } else {
        items.push(PrecheckItem::warning(
            "CC-Switch 数据库",
            "未找到 CC-Switch 数据库，仅使用 CLI 配置中的 Provider",
            "如需统一管理 Provider，请安装并配置 CC-Switch",
            None,
        ));
    }

    if !facts.codex_enabled {
        items.push(PrecheckItem::skip("Codex 配置文件", "Codex 未启用"));
    } else if facts.codex_config_exists {
        items.push(PrecheckItem::pass("Codex 配置文件", "Codex 配置文件存在"));
    } else {
        items.push(PrecheckItem::warning(
            "Codex 配置文件",
            "Codex 配置文件不存在，同步时会自动创建",
            "无需处理；执行同步后自动生成",
            Some(PrecheckAction::SyncRoutes),
        ));
    }

    if !facts.claude_enabled {
        items.push(PrecheckItem::skip("Claude 配置文件", "Claude 未启用"));
    } else if facts.claude_settings_exists {
        items.push(PrecheckItem::pass("Claude 配置文件", "Claude 配置文件存在"));
    } else {
        items.push(PrecheckItem::warning(
            "Claude 配置文件",
            "Claude 配置文件不存在，同步时会自动创建",
            "无需处理；执行同步后自动生成",
            Some(PrecheckAction::SyncRoutes),
        ));
    }

    if !facts.mode_needs_headroom {
        items.push(PrecheckItem::skip(
            "端口冲突",
            "当前模式不使用 headroom_port，无需检查端口",
        ));
    } else if facts.agent_port == facts.headroom_port {
        items.push(PrecheckItem::fail(
            "端口冲突",
            format!(
                "agent_port（{}）与 headroom_port（{}）相同，会互相抢占端口",
                facts.agent_port, facts.headroom_port
            ),
            "请修改 config.json 中的 agent_port 或 headroom_port 为不同值",
            Some(PrecheckAction::OpenConfig),
        ));
    } else {
        items.push(PrecheckItem::pass(
            "端口冲突",
            format!(
                "agent_port（{}）与 headroom_port（{}）不冲突",
                facts.agent_port, facts.headroom_port
            ),
        ));
    }

    let runtime_status = evaluate_runtime_status(RuntimeStatusInput {
        codex_enabled: facts.codex_enabled,
        claude_enabled: facts.claude_enabled,
        direct_codex: facts.direct_codex,
        direct_claude: facts.direct_claude,
        bypass_headroom: facts.bypass_headroom,
        codex_route_health: facts.codex_has_route.then_some(RouteHealth::Healthy),
        claude_route_health: facts.claude_has_route.then_some(RouteHealth::Healthy),
        headroom_state: if !facts.mode_needs_headroom {
            "external"
        } else if facts.python_found {
            "healthy"
        } else {
            "runtime-unavailable"
        },
        sync_in_progress: false,
        restart_in_progress: false,
        recovery_in_progress: false,
    });

    PrecheckReport {
        items,
        runtime_status,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn find<'a>(report: &'a PrecheckReport, name: &str) -> &'a PrecheckItem {
        report
            .items
            .iter()
            .find(|item| item.name == name)
            .unwrap_or_else(|| panic!("缺少预检项: {name}"))
    }

    #[test]
    fn healthy_configuration_passes_all_checks() {
        let report = evaluate(&pass_facts(), "advice");
        assert_eq!(report.items.len(), 7);
        assert!(
            report
                .items
                .iter()
                .all(|item| item.status == PrecheckStatus::Pass)
        );
        assert_eq!(report.count(PrecheckStatus::Pass), 7);
        assert_eq!(
            report.summary_line(),
            "预检结果：通过 7，警告 0，失败 0，跳过 0"
        );
    }

    #[test]
    fn missing_required_headroom_fails_with_repair_advice() {
        let mut facts = pass_facts();
        facts.python_found = false;
        let advice = "请在 PowerShell 运行：python -m pip install headroom-ai[code]==0.34.0";
        let report = evaluate(&facts, advice);
        let item = find(&report, "Headroom 运行环境");
        assert_eq!(item.status, PrecheckStatus::Fail);
        assert!(item.advice.contains("headroom-ai[code]==0.34.0"));
    }

    #[test]
    fn bypass_mode_without_headroom_does_not_fail() {
        let mut facts = pass_facts();
        facts.mode_needs_headroom = false;
        facts.python_found = false;
        let report = evaluate(&facts, "advice");
        assert_eq!(
            find(&report, "Headroom 运行环境").status,
            PrecheckStatus::Skip
        );
        assert_eq!(find(&report, "端口冲突").status, PrecheckStatus::Skip);
        assert!(
            !report
                .items
                .iter()
                .any(|item| item.status == PrecheckStatus::Fail)
        );
    }

    #[test]
    fn enabled_protocol_without_route_fails() {
        let mut facts = pass_facts();
        facts.codex_has_route = false;
        let report = evaluate(&facts, "advice");
        let item = find(&report, "Codex 路由");
        assert_eq!(item.status, PrecheckStatus::Fail);
        assert!(item.advice.contains("同步"));
    }

    #[test]
    fn disabled_protocol_is_skipped() {
        let mut facts = pass_facts();
        facts.codex_enabled = false;
        let report = evaluate(&facts, "advice");
        assert_eq!(find(&report, "Codex 路由").status, PrecheckStatus::Skip);
        assert_eq!(find(&report, "Codex 配置文件").status, PrecheckStatus::Skip);
    }

    #[test]
    fn agent_and_headroom_port_collision_fails() {
        let mut facts = pass_facts();
        facts.agent_port = 8787;
        facts.headroom_port = 8787;
        let report = evaluate(&facts, "advice");
        let item = find(&report, "端口冲突");
        assert_eq!(item.status, PrecheckStatus::Fail);
        assert!(item.advice.contains("agent_port"));
    }

    #[test]
    fn headroom_needed_only_when_managing_upstream() {
        assert!(!mode_needs_headroom(&AppConfig::default()));
        assert!(mode_needs_headroom(&AppConfig {
            manage_upstream: true,
            ..AppConfig::default()
        }));
        assert!(!mode_needs_headroom(&AppConfig {
            manage_upstream: true,
            bypass_headroom: true,
            ..AppConfig::default()
        }));
        assert!(!mode_needs_headroom(&AppConfig {
            manage_upstream: true,
            enable_codex: false,
            enable_claude: false,
            ..AppConfig::default()
        }));
        assert!(mode_needs_headroom(&AppConfig {
            manage_upstream: true,
            enable_claude: false,
            ..AppConfig::default()
        }));
    }

    #[test]
    fn port_conflict_is_skipped_when_headroom_not_needed() {
        let mut facts = pass_facts();
        facts.mode_needs_headroom = false;
        facts.agent_port = 8787;
        facts.headroom_port = 8787;
        let report = evaluate(&facts, "advice");
        assert_eq!(
            find(&report, "Headroom 运行环境").status,
            PrecheckStatus::Skip
        );
        assert_eq!(find(&report, "端口冲突").status, PrecheckStatus::Skip);
        assert!(
            !report
                .items
                .iter()
                .any(|item| item.status == PrecheckStatus::Fail)
        );
    }

    #[test]
    fn healthy_report_has_no_repair_actions() {
        let report = evaluate(&pass_facts(), "advice");
        assert!(report.actions().is_empty());
        assert_eq!(report.count(PrecheckStatus::Fail), 0);
        assert_eq!(
            report.runtime_status.mode,
            crate::model::RuntimeMode::Normal
        );
        assert!(report.to_text().contains("运行结论：正常"));
    }

    #[test]
    fn precheck_runtime_conclusion_uses_the_same_bypass_model() {
        let mut facts = pass_facts();
        facts.mode_needs_headroom = false;
        facts.python_found = false;
        facts.bypass_headroom = true;
        let report = evaluate(&facts, "advice");
        assert_eq!(
            report.runtime_status.mode,
            crate::model::RuntimeMode::Bypass
        );
        assert_eq!(
            report.runtime_status.headroom.state,
            crate::model::ComponentState::NotRequired
        );
    }

    #[test]
    fn same_kind_codex_and_claude_issue_produces_single_sync_action() {
        let mut facts = pass_facts();
        facts.codex_has_route = false;
        facts.claude_has_route = false;
        facts.codex_config_exists = false;
        facts.claude_settings_exists = false;
        let report = evaluate(&facts, "advice");
        let actions = report.actions();
        assert_eq!(
            actions,
            vec![PrecheckAction::SyncRoutes],
            "同类 Codex/Claude 问题只应出现一个同步动作，实际: {actions:?}"
        );
    }

    #[test]
    fn runtime_and_port_failures_map_to_their_own_actions() {
        let mut facts = pass_facts();
        facts.python_found = false;
        facts.agent_port = 8787;
        facts.headroom_port = 8787;
        let report = evaluate(&facts, "advice");
        assert_eq!(
            report.actions(),
            vec![PrecheckAction::SelectPython, PrecheckAction::OpenConfig]
        );
    }

    #[test]
    fn missing_optional_cc_switch_database_adds_no_action() {
        let mut facts = pass_facts();
        facts.cc_switch_db_exists = false;
        let report = evaluate(&facts, "advice");
        assert!(
            report.actions().is_empty(),
            "CC-Switch 缺失是可选警告，不应提供破坏性或安装动作"
        );
    }

    #[test]
    fn action_mapping_does_not_parse_chinese_description_text() {
        let mut facts = pass_facts();
        facts.codex_has_route = false;
        let report = evaluate(&facts, "advice");
        assert!(report.actions().contains(&PrecheckAction::SyncRoutes));
        assert!(
            report
                .items
                .iter()
                .any(|item| item.description.contains("Codex 已启用"))
        );
    }

    #[test]
    fn collect_does_not_probe_python_when_headroom_not_needed() {
        let dir = std::env::temp_dir().join(format!("hr-precheck-nohr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("state")).unwrap();
        let config = AppConfig {
            codex_config: dir.join("codex.toml"),
            claude_settings: dir.join("settings.json"),
            cc_switch_db: dir.join("cc-switch.db"),
            state_dir: dir.join("state"),
            legacy_state_dir: dir.join("legacy"),
            enable_codex: false,
            enable_claude: false,
            agent_port: 8787,
            headroom_port: 8787,
            ..AppConfig::default()
        };
        let report = collect(&config);
        assert_eq!(
            find(&report, "Headroom 运行环境").status,
            PrecheckStatus::Skip
        );
        assert_eq!(find(&report, "端口冲突").status, PrecheckStatus::Skip);
        assert_eq!(report.count(PrecheckStatus::Fail), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn collected_report_omits_secret_from_config_files() {
        let secret = "sk-test-secret-0123456789abcdef";
        let dir = std::env::temp_dir().join(format!("hr-precheck-{}", std::process::id()));
        let codex = dir.join("codex.toml");
        std::fs::create_dir_all(dir.join("state")).unwrap();
        std::fs::write(
            &codex,
            format!(
                "model_provider = \"provider-a\"\n\n[model_providers.provider_a]\nname = \"Provider A\"\nbase_url = \"https://api.example.com/v1\"\n\n[model_providers.provider_a.env]\nOPENAI_API_KEY = \"{secret}\"\n"
            ),
        )
        .unwrap();
        let config = AppConfig {
            codex_config: codex,
            claude_settings: dir.join("settings.json"),
            cc_switch_db: dir.join("cc-switch.db"),
            state_dir: dir.join("state"),
            legacy_state_dir: dir.join("legacy"),
            headroom_python: Some(dir.join("missing-python.exe")),
            enable_codex: true,
            enable_claude: false,
            ..AppConfig::default()
        };
        let report = collect_with(&config, false);
        let text = report.to_text();
        assert!(!text.contains(secret));
        assert!(!text.contains("provider-a"));
        assert!(!text.contains("OPENAI_API_KEY"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
