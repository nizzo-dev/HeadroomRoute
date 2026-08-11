use super::RECT;

const PRECHECK_BASE_WIDTH: i32 = 780;
const PRECHECK_BASE_HEIGHT: i32 = 600;
const PRECHECK_MARGIN: i32 = 24;
const PRECHECK_TOP_MARGIN: i32 = 16;
const PRECHECK_BOTTOM_MARGIN: i32 = 24;
const PRECHECK_TITLE_WIDTH: i32 = 400;
const PRECHECK_TITLE_HEIGHT: i32 = 30;
const PRECHECK_SUMMARY_HEIGHT: i32 = 24;
const PRECHECK_VERTICAL_GAP: i32 = 8;
const PRECHECK_ACTION_WIDTH: i32 = 232;
const PRECHECK_ACTION_HEIGHT: i32 = 30;
const PRECHECK_ACTION_GAP: i32 = 10;
const PRECHECK_FOOTER_HEIGHT: i32 = 32;
const PRECHECK_FOOTER_GAP: i32 = 14;
const PRECHECK_RECHECK_WIDTH: i32 = 110;
const PRECHECK_COPY_WIDTH: i32 = 110;
const PRECHECK_CLOSE_WIDTH: i32 = 84;
/// 紧凑布局基线（工作区放不下完整三列按钮或纵向内容时使用）：收紧边距、行距、
/// 标题与按钮高度，动作按钮按可用宽度均分，底部按钮换更窄标签所需的最小宽度。
/// 标签由 `precheck_action_compact_label` 提供，保证极小工作区内文本不被裁切。
const PRECHECK_COMPACT_MARGIN: i32 = 12;
const PRECHECK_COMPACT_TOP_MARGIN: i32 = 8;
const PRECHECK_COMPACT_BOTTOM_MARGIN: i32 = 12;
const PRECHECK_COMPACT_TITLE_HEIGHT: i32 = 22;
const PRECHECK_COMPACT_SUMMARY_HEIGHT: i32 = 20;
const PRECHECK_COMPACT_VERTICAL_GAP: i32 = 4;
const PRECHECK_COMPACT_ACTION_HEIGHT: i32 = 26;
const PRECHECK_COMPACT_FOOTER_GAP: i32 = 8;
const PRECHECK_COMPACT_RECHECK_WIDTH: i32 = 72;
const PRECHECK_COMPACT_COPY_WIDTH: i32 = 72;
const PRECHECK_COMPACT_CLOSE_WIDTH: i32 = 48;
/// 能完整容纳紧凑控件的最小客户区。低于这个物理尺寸时窗口采用此下限，避免
/// 控件出现负坐标或相互覆盖；这类工作区无法保证窗口边框也完整落入工作区。
const PRECHECK_MIN_CLIENT_WIDTH: i32 = 240;
const PRECHECK_MIN_CLIENT_HEIGHT: i32 = 180;
const PRECHECK_BODY_FONT_HEIGHT: i32 = 15;
const PRECHECK_TITLE_FONT_HEIGHT: i32 = 22;

/// 预检窗口内一个控件在客户区中的矩形（像素）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PrecheckRect {
    pub(super) x: i32,
    pub(super) y: i32,
    pub(super) width: i32,
    pub(super) height: i32,
}

#[cfg(test)]
impl PrecheckRect {
    pub(super) fn right(self) -> i32 {
        self.x + self.width
    }

    pub(super) fn bottom(self) -> i32 {
        self.y + self.height
    }
}

/// 预检窗口纯布局结果：完整窗口的位置与尺寸、各控件客户区矩形和字体像素高度。
/// 由 [`precheck_layout`] 一次性算出，供窗口创建与控件摆放共用同一套坐标。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PrecheckLayout {
    pub(super) window_x: i32,
    pub(super) window_y: i32,
    pub(super) window_width: i32,
    pub(super) window_height: i32,
    pub(super) client_width: i32,
    pub(super) client_height: i32,
    pub(super) title: PrecheckRect,
    pub(super) summary: PrecheckRect,
    pub(super) report: PrecheckRect,
    pub(super) actions: [PrecheckRect; 3],
    pub(super) recheck: PrecheckRect,
    pub(super) copy: PrecheckRect,
    pub(super) close: PrecheckRect,
    pub(super) body_font: i32,
    pub(super) title_font: i32,
    /// 工作区放不下完整布局时为真；此时使用更小的间距与按钮，动作按钮标签
    /// 由 [`precheck_action_compact_label`] 提供。
    pub(super) compact: bool,
}

/// 96 DPI 基准值按给定 DPI 缩放，四舍五入到整像素。DPI 低于 96 时按 96 处理。
pub(super) fn precheck_scale(value: i32, dpi: u32) -> i32 {
    ((value as i64 * dpi.max(96) as i64 + 48) / 96) as i32
}

/// 纯布局计算：按 DPI 缩放窗口与控件，并把窗口限制在 owner 显示器工作区内。
///
/// - 完整窗口尺寸 = 客户区 + `frame_width`/`frame_height`（由
///   `AdjustWindowRectEx` 得到的标题栏与边框），窗口整体始终不超过工作区；
/// - 报告编辑区吸收剩余高度；动作按钮与底部按钮保持完整尺寸、互不重叠；
/// - 工作区放不下完整三列动作按钮或纵向内容时进入紧凑布局：收紧边距与行距、
///   缩短按钮（配合 `precheck_action_compact_label` 的短标签）、动作按钮按可用
///   宽度均分，保证控件仍互不重叠且不被裁切。
pub(super) fn precheck_layout(
    dpi: u32,
    work_area: RECT,
    frame_width: i32,
    frame_height: i32,
) -> PrecheckLayout {
    let scale = |value: i32| precheck_scale(value, dpi);
    let margin = scale(PRECHECK_MARGIN);
    let m_top = scale(PRECHECK_TOP_MARGIN);
    let m_bottom = scale(PRECHECK_BOTTOM_MARGIN);
    let gap = scale(PRECHECK_VERTICAL_GAP);
    let title_h = scale(PRECHECK_TITLE_HEIGHT);
    let summary_h = scale(PRECHECK_SUMMARY_HEIGHT);
    let action_w = scale(PRECHECK_ACTION_WIDTH);
    let action_h = scale(PRECHECK_ACTION_HEIGHT);
    let action_gap = scale(PRECHECK_ACTION_GAP);
    let footer_h = scale(PRECHECK_FOOTER_HEIGHT);
    let footer_gap = scale(PRECHECK_FOOTER_GAP);

    let desired_w = scale(PRECHECK_BASE_WIDTH);
    let desired_h = scale(PRECHECK_BASE_HEIGHT);

    let work_w = work_area.right - work_area.left;
    let work_h = work_area.bottom - work_area.top;
    // 客户区上限 = 工作区减去 frame。达到最小支持尺寸时，完整窗口（含 frame）
    // 始终落在工作区内；物理工作区更小时采用明确的最小客户区，优先保证控件有效。
    let available_w = (work_w - frame_width).max(1);
    let available_h = (work_h - frame_height).max(1);
    let min_client_w = scale(PRECHECK_MIN_CLIENT_WIDTH);
    let min_client_h = scale(PRECHECK_MIN_CLIENT_HEIGHT);

    let client_w = desired_w.min(available_w.max(min_client_w));
    let client_h = desired_h.min(available_h.max(min_client_h));

    let window_width = client_w + frame_width;
    let window_height = client_h + frame_height;
    // 窗口能放进工作区时居中，放不下时从工作区左上角开始（`.max(0)` 保证偏移不为负）。
    let window_x = work_area.left + (work_w - window_width).max(0) / 2;
    let window_y = work_area.top + (work_h - window_height).max(0) / 2;

    // 完整三列动作按钮行所需的最小客户区宽度；放不下时进入紧凑布局。
    let wide_min_w = margin * 2 + action_w * 3 + action_gap * 2;
    // 完整纵向内容所需的最小客户区高度；放不下时进入紧凑布局。
    let report_top = m_top + title_h + gap + summary_h + gap;
    let wide_min_h = report_top + gap + action_h + footer_gap + footer_h + m_bottom;
    let compact = client_w < wide_min_w || client_h < wide_min_h;

    // 紧凑布局：收紧边距、行距、标题与按钮高度；动作按钮按可用宽度均分。
    let (margin, m_top, m_bottom, gap, title_h, summary_h, action_h, footer_gap) = if compact {
        (
            scale(PRECHECK_COMPACT_MARGIN),
            scale(PRECHECK_COMPACT_TOP_MARGIN),
            scale(PRECHECK_COMPACT_BOTTOM_MARGIN),
            scale(PRECHECK_COMPACT_VERTICAL_GAP),
            scale(PRECHECK_COMPACT_TITLE_HEIGHT),
            scale(PRECHECK_COMPACT_SUMMARY_HEIGHT),
            scale(PRECHECK_COMPACT_ACTION_HEIGHT),
            scale(PRECHECK_COMPACT_FOOTER_GAP),
        )
    } else {
        (
            margin, m_top, m_bottom, gap, title_h, summary_h, action_h, footer_gap,
        )
    };

    let inner_width = client_w - margin * 2;
    let report_top = m_top + title_h + gap + summary_h + gap;
    let title = PrecheckRect {
        x: margin,
        y: m_top,
        width: scale(PRECHECK_TITLE_WIDTH).min(inner_width.max(0)),
        height: title_h,
    };
    let summary = PrecheckRect {
        x: margin,
        y: m_top + title_h + gap,
        width: inner_width,
        height: summary_h,
    };
    let content_bottom = client_h - m_bottom;
    let footer_top = content_bottom - footer_h;
    let action_top = footer_top - footer_gap - action_h;
    let report_bottom = action_top - gap;
    let report = PrecheckRect {
        x: margin,
        y: report_top,
        width: inner_width,
        height: report_bottom.saturating_sub(report_top),
    };

    // 动作按钮宽度：紧凑时按可用宽度均分（不超过完整布局宽度），普通布局用基准宽度。
    let action_w = if compact {
        (inner_width.saturating_sub(action_gap * 2) / 3)
            .min(action_w)
            .max(1)
    } else {
        action_w
    };
    let actions_total = action_w * 3 + action_gap * 2;
    let actions_left = margin + (inner_width - actions_total).max(0) / 2;
    let mut actions = [PrecheckRect {
        x: 0,
        y: 0,
        width: 0,
        height: 0,
    }; 3];
    for (slot, rect) in actions.iter_mut().enumerate() {
        let x = actions_left + slot as i32 * (action_w + action_gap);
        *rect = PrecheckRect {
            x,
            y: action_top,
            width: action_w,
            height: action_h,
        };
    }

    let (recheck_w, copy_w, close_w) = if compact {
        (
            scale(PRECHECK_COMPACT_RECHECK_WIDTH),
            scale(PRECHECK_COMPACT_COPY_WIDTH),
            scale(PRECHECK_COMPACT_CLOSE_WIDTH),
        )
    } else {
        (
            scale(PRECHECK_RECHECK_WIDTH),
            scale(PRECHECK_COPY_WIDTH),
            scale(PRECHECK_CLOSE_WIDTH),
        )
    };
    let recheck = PrecheckRect {
        x: margin,
        y: footer_top,
        width: recheck_w,
        height: footer_h,
    };
    let copy = PrecheckRect {
        x: margin + recheck_w + gap,
        y: footer_top,
        width: copy_w,
        height: footer_h,
    };
    let close = PrecheckRect {
        x: margin + inner_width - close_w,
        y: footer_top,
        width: close_w,
        height: footer_h,
    };

    PrecheckLayout {
        window_x,
        window_y,
        window_width,
        window_height,
        client_width: client_w,
        client_height: client_h,
        title,
        summary,
        report,
        actions,
        recheck,
        copy,
        close,
        body_font: -scale(PRECHECK_BODY_FONT_HEIGHT),
        title_font: -scale(PRECHECK_TITLE_FONT_HEIGHT),
        compact,
    }
}
