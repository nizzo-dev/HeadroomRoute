use super::{
    ID_CONFIG, ID_RESTART, ID_SELECT_RUNTIME, ID_SYNC, approval_allow_rect, approval_deny_rect,
    approval_ease, approval_lerp, approval_scale, compact_number, failover_sources,
    hover_popup_size, move_target, precheck_action_command, precheck_action_compact_label,
    precheck_action_label, precheck_layout, precheck_scale, recommended_action, route_hover_text,
    route_is_selected,
};
use crate::model::{
    AuthStyle, FailoverPolicy, Protocol, Route, RouteHealth, RuntimeStatusInput,
    evaluate_runtime_status,
};
use crate::precheck::PrecheckAction;
use windows_sys::Win32::Foundation::RECT;

#[test]
fn precheck_layout_scales_with_dpi() {
    let work = RECT {
        left: 0,
        top: 0,
        right: 4096,
        bottom: 2160,
    };
    for dpi in [96, 144, 192] {
        let layout = precheck_layout(dpi, work, 0, 0);
        let n = dpi as i32;
        assert_eq!(
            layout.client_width,
            780 * n / 96,
            "DPI {dpi}: 客户区宽度应按比例缩放"
        );
        assert_eq!(
            layout.client_height,
            600 * n / 96,
            "DPI {dpi}: 客户区高度应按比例缩放"
        );
        assert_eq!(layout.title.x, precheck_scale(24, dpi));
        assert_eq!(layout.title.y, precheck_scale(16, dpi));
        assert_eq!(layout.title.width, precheck_scale(400, dpi));
        assert_eq!(layout.title.height, precheck_scale(30, dpi));
        assert_eq!(layout.body_font, -precheck_scale(15, dpi));
        assert_eq!(layout.title_font, -precheck_scale(22, dpi));
    }
}

#[test]
fn precheck_layout_centers_window_in_work_area() {
    let work = RECT {
        left: 100,
        top: 50,
        right: 1100,
        bottom: 750,
    };
    let layout = precheck_layout(96, work, 0, 0);
    assert_eq!(layout.window_x, 100 + (1000 - 780) / 2);
    assert_eq!(layout.window_y, 50 + (700 - 600) / 2);
    assert!(layout.window_x >= work.left);
    assert!(layout.window_y >= work.top);
    assert!(layout.window_x + layout.window_width <= work.right);
    assert!(layout.window_y + layout.window_height <= work.bottom);
}

#[test]
fn precheck_layout_fits_window_including_frame() {
    let work = RECT {
        left: 0,
        top: 0,
        right: 1024,
        bottom: 768,
    };
    let layout = precheck_layout(96, work, 8, 39);
    assert!(layout.window_x + layout.window_width <= work.right);
    assert!(layout.window_y + layout.window_height <= work.bottom);
    assert!(layout.window_width >= 780);
}

#[test]
fn precheck_report_adapts_to_smaller_work_area() {
    let large = precheck_layout(
        96,
        RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        },
        0,
        0,
    );
    let small = precheck_layout(
        96,
        RECT {
            left: 0,
            top: 0,
            right: 800,
            bottom: 500,
        },
        0,
        0,
    );
    assert!(large.report.height > small.report.height);
    assert!(small.report.height > 0);
    assert_eq!(small.client_width, 780);
    assert_eq!(small.client_height, 500);
}

#[test]
fn precheck_buttons_never_overlap_or_clip() {
    let cases = [
        (
            96,
            RECT {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1080,
            },
        ),
        (
            144,
            RECT {
                left: 0,
                top: 0,
                right: 2560,
                bottom: 1440,
            },
        ),
        (
            192,
            RECT {
                left: 0,
                top: 0,
                right: 3840,
                bottom: 2160,
            },
        ),
        (
            96,
            RECT {
                left: 0,
                top: 0,
                right: 800,
                bottom: 600,
            },
        ),
        (
            96,
            RECT {
                left: 0,
                top: 0,
                right: 500,
                bottom: 400,
            },
        ),
        (
            144,
            RECT {
                left: 0,
                top: 0,
                right: 640,
                bottom: 480,
            },
        ),
    ];
    for (dpi, work) in cases {
        let layout = precheck_layout(dpi, work, 0, 0);
        // 完整窗口必须落在工作区内。
        assert!(
            layout.window_x >= work.left && layout.window_y >= work.top,
            "DPI {dpi} 窗口超出工作区左上边界"
        );
        assert!(
            layout.window_x + layout.window_width <= work.right,
            "DPI {dpi} 窗口超出工作区右边界"
        );
        assert!(
            layout.window_y + layout.window_height <= work.bottom,
            "DPI {dpi} 窗口超出工作区下边界"
        );
        // 文本控件必须完整可见。
        assert!(
            layout.title.right() <= layout.client_width
                && layout.summary.right() <= layout.client_width
                && layout.report.right() <= layout.client_width,
            "DPI {dpi} 文本控件超出右边界"
        );
        for (index, rect) in layout.actions.iter().enumerate() {
            assert!(rect.x >= 0, "DPI {dpi} 动作按钮 {index} 越界左");
            assert!(rect.y >= 0, "DPI {dpi} 动作按钮 {index} 越界上");
            assert!(
                rect.right() <= layout.client_width,
                "DPI {dpi} 动作按钮 {index} 超出右边界"
            );
            assert!(
                rect.bottom() <= layout.client_height,
                "DPI {dpi} 动作按钮 {index} 超出下边界"
            );
            assert!(rect.width > 0 && rect.height > 0);
        }
        for index in 0..2 {
            assert!(
                layout.actions[index].right() <= layout.actions[index + 1].x,
                "DPI {dpi} 动作按钮 {index} 与 {} 重叠",
                index + 1
            );
        }
        for (name, rect) in [
            ("recheck", layout.recheck),
            ("copy", layout.copy),
            ("close", layout.close),
        ] {
            assert!(
                rect.right() <= layout.client_width,
                "DPI {dpi} 底部按钮 {name} 超出右边界"
            );
            assert!(
                rect.bottom() <= layout.client_height,
                "DPI {dpi} 底部按钮 {name} 超出下边界"
            );
        }
        assert!(
            layout.copy.x >= layout.recheck.right(),
            "DPI {dpi} 底部按钮互相重叠"
        );
        assert!(
            layout.close.x >= layout.copy.right(),
            "DPI {dpi} 底部按钮互相重叠"
        );
    }
}

#[test]
fn precheck_layout_fits_tiny_work_area_including_frame() {
    let tiny = RECT {
        left: 10,
        top: 20,
        right: 310,
        bottom: 270,
    };
    let layout = precheck_layout(96, tiny, 8, 39);
    // 完整窗口（含 frame）必须始终不超过工作区。
    assert!(
        layout.window_x >= tiny.left && layout.window_y >= tiny.top,
        "窗口不能超出工作区左上边界"
    );
    assert!(
        layout.window_x + layout.window_width <= tiny.right,
        "窗口不能超出工作区右边界"
    );
    assert!(
        layout.window_y + layout.window_height <= tiny.bottom,
        "窗口不能超出工作区下边界"
    );
    assert!(layout.compact, "极小工作区应使用紧凑布局");
    // 动作按钮与底部按钮必须完整可见、尺寸有效且互不重叠。
    for (index, rect) in layout.actions.iter().enumerate() {
        assert!(
            rect.width > 0 && rect.height > 0,
            "动作按钮 {index} 尺寸无效"
        );
        assert!(rect.x >= 0 && rect.y >= 0, "动作按钮 {index} 越界");
        assert!(
            rect.right() <= layout.client_width,
            "动作按钮 {index} 超出右边界"
        );
        assert!(
            rect.bottom() <= layout.client_height,
            "动作按钮 {index} 超出下边界"
        );
    }
    for index in 0..2 {
        assert!(
            layout.actions[index].right() <= layout.actions[index + 1].x,
            "动作按钮 {index} 与 {} 重叠",
            index + 1
        );
    }
    for (name, rect) in [
        ("recheck", layout.recheck),
        ("copy", layout.copy),
        ("close", layout.close),
    ] {
        assert!(
            rect.width > 0 && rect.height > 0,
            "底部按钮 {name} 尺寸无效"
        );
        assert!(
            rect.right() <= layout.client_width,
            "底部按钮 {name} 超出右边界"
        );
        assert!(
            rect.bottom() <= layout.client_height,
            "底部按钮 {name} 超出下边界"
        );
    }
    assert!(layout.copy.x >= layout.recheck.right(), "底部按钮互相重叠");
    assert!(layout.close.x >= layout.copy.right(), "底部按钮互相重叠");
    // 文本控件必须完整可见。
    assert!(layout.title.width > 0 && layout.summary.width > 0);
    assert!(layout.title.right() <= layout.client_width, "标题被裁切");
    assert!(layout.summary.right() <= layout.client_width, "摘要被裁切");
    assert!(layout.report.height >= 0, "报告区高度无效");
}

#[test]
fn precheck_action_compact_labels_are_short() {
    assert_eq!(
        precheck_action_compact_label(PrecheckAction::SelectPython),
        "选择"
    );
    assert_eq!(
        precheck_action_compact_label(PrecheckAction::SyncRoutes),
        "同步"
    );
    assert_eq!(
        precheck_action_compact_label(PrecheckAction::OpenConfig),
        "配置"
    );
}

#[test]
fn precheck_scale_rounds_to_integer_pixels() {
    assert_eq!(precheck_scale(15, 96), 15);
    assert_eq!(precheck_scale(15, 144), 23);
    assert_eq!(precheck_scale(15, 192), 30);
    assert_eq!(precheck_scale(24, 96), 24);
    assert_eq!(precheck_scale(0, 144), 0);
}

#[test]
fn precheck_action_buttons_route_to_existing_tray_commands() {
    assert_eq!(precheck_action_command(0), Some(ID_SELECT_RUNTIME));
    assert_eq!(precheck_action_command(1), Some(ID_SYNC));
    assert_eq!(precheck_action_command(2), Some(ID_CONFIG));
    assert_eq!(precheck_action_command(3), None);
    assert_eq!(precheck_action_command(99), None);
    assert_eq!(
        precheck_action_label(PrecheckAction::SelectPython),
        "选择 Headroom Python..."
    );
    assert_eq!(
        precheck_action_label(PrecheckAction::SyncRoutes),
        "同步 Codex + Claude / CC-Switch"
    );
    assert_eq!(
        precheck_action_label(PrecheckAction::OpenConfig),
        "打开 config.json"
    );
}

#[test]
fn duplicate_urls_select_only_the_active_provider() {
    let first = Route::new(
        Protocol::OpenAi,
        "first".into(),
        "First".into(),
        "https://same.example.com/v1".into(),
        Some("key-a".into()),
        AuthStyle::Bearer,
        "test",
    );
    let second = Route::new(
        Protocol::OpenAi,
        "second".into(),
        "Second".into(),
        "https://same.example.com/v1".into(),
        Some("key-b".into()),
        AuthStyle::Bearer,
        "test",
    );
    assert!(!route_is_selected(&first, Some("second")));
    assert!(route_is_selected(&second, Some("second")));
}

#[test]
fn compacts_large_status_numbers() {
    assert_eq!(compact_number(999), "999");
    assert_eq!(compact_number(1_000), "1K");
    assert_eq!(compact_number(12_345), "12.3K");
    assert_eq!(compact_number(1_000_000), "1M");
}

#[test]
fn wraps_long_api_keys_without_losing_characters() {
    let key = "a".repeat(130);
    let route = Route::new(
        Protocol::OpenAi,
        "provider".into(),
        "Provider".into(),
        "https://example.com/v1".into(),
        Some(key.clone()),
        AuthStyle::Bearer,
        "test",
    );
    let text = route_hover_text(&route, true);
    let displayed = text
        .lines()
        .skip(1)
        .collect::<String>()
        .strip_prefix("API Key：")
        .unwrap()
        .to_owned();
    assert_eq!(displayed, key);
    assert!(hover_popup_size(&text).1 > 48);
}

#[test]
fn recommends_the_action_closest_to_the_fault() {
    let input = RuntimeStatusInput {
        codex_enabled: true,
        claude_enabled: true,
        direct_codex: false,
        direct_claude: false,
        bypass_headroom: false,
        codex_route_health: Some(RouteHealth::Healthy),
        claude_route_health: Some(RouteHealth::Healthy),
        headroom_state: "runtime-unavailable",
        sync_in_progress: false,
        restart_in_progress: false,
        recovery_in_progress: false,
    };
    let status = evaluate_runtime_status(input);
    assert_eq!(
        recommended_action(&status, "runtime-unavailable", None).map(|action| action.0),
        Some(ID_SELECT_RUNTIME)
    );
    let status = evaluate_runtime_status(RuntimeStatusInput {
        headroom_state: "unavailable",
        ..input
    });
    assert_eq!(
        recommended_action(&status, "unavailable", None).map(|action| action.0),
        Some(ID_RESTART)
    );
}

#[test]
fn moves_failover_targets_without_crossing_list_bounds() {
    let mut targets = vec!["one".into(), "two".into(), "three".into()];
    assert_eq!(move_target(&mut targets, 1, -1), Some(0));
    assert_eq!(targets, ["two", "one", "three"]);
    assert_eq!(move_target(&mut targets, 0, -1), None);
    assert_eq!(move_target(&mut targets, 2, 1), None);
}

#[test]
fn approval_animation_uses_bounded_easing() {
    assert_eq!(approval_ease(0.0), 0.0);
    assert_eq!(approval_ease(1.0), 1.0);
    assert!(approval_ease(0.5) > 0.5);
    assert_eq!(approval_lerp(280, 520, 0.5), 400);
    assert_eq!(approval_scale(520, 144), 780);
}

#[test]
fn approval_buttons_keep_dpi_scaled_margins() {
    let normal_allow = approval_allow_rect(520, 286, 96);
    let large_allow = approval_allow_rect(780, 429, 144);
    assert_eq!(normal_allow.right, 502);
    assert_eq!(large_allow.right, 753);
    assert_eq!(normal_allow.bottom, 268);
    assert_eq!(large_allow.bottom, 402);
    assert_eq!(
        approval_deny_rect(520, 286, 96).right,
        normal_allow.left - 10
    );
}

#[test]
fn editor_keeps_stale_configured_sources_visible() {
    let route = Route::new(
        Protocol::OpenAi,
        "active".into(),
        "Active".into(),
        "https://active.example.com/v1".into(),
        None,
        AuthStyle::PassThrough,
        "test",
    );
    let mut policy = FailoverPolicy::default();
    policy.openai.insert("deleted".into(), Vec::new());
    assert_eq!(
        failover_sources(&[route], &policy, Protocol::OpenAi),
        ["active", "deleted"]
    );
}
