use super::*;

#[allow(unsafe_op_in_unsafe_fn)]
impl FailoverEditor {
    pub(super) unsafe fn create_controls(&mut self, hwnd: HWND) {
        let stock_font = GetStockObject(DEFAULT_GUI_FONT) as usize;
        let instance = GetModuleHandleW(ptr::null());
        self.body_font = CreateFontW(
            -15,
            0,
            0,
            0,
            FW_NORMAL as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            OUT_DEFAULT_PRECIS as u32,
            CLIP_DEFAULT_PRECIS as u32,
            CLEARTYPE_QUALITY as u32,
            (DEFAULT_PITCH | FF_DONTCARE) as u32,
            wide("Segoe UI").as_ptr(),
        ) as usize;
        let font = if self.body_font == 0 {
            stock_font
        } else {
            self.body_font
        };
        self.title_font = CreateFontW(
            -22,
            0,
            0,
            0,
            FW_SEMIBOLD as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            OUT_DEFAULT_PRECIS as u32,
            CLIP_DEFAULT_PRECIS as u32,
            CLEARTYPE_QUALITY as u32,
            (DEFAULT_PITCH | FF_DONTCARE) as u32,
            wide("Segoe UI").as_ptr(),
        ) as usize;
        let title_font = if self.title_font == 0 {
            font
        } else {
            self.title_font
        };
        editor_control(
            hwnd,
            "STATIC",
            "故障转移策略",
            24,
            16,
            420,
            30,
            0,
            WS_CHILD | WS_VISIBLE,
            instance,
            title_font,
        );
        editor_control(
            hwnd,
            "STATIC",
            "为每个 Provider 指定允许转移的目标，并按优先级从上到下依次尝试。",
            24,
            48,
            720,
            20,
            0,
            WS_CHILD | WS_VISIBLE,
            instance,
            font,
        );
        editor_control(
            hwnd,
            "BUTTON",
            "启用自动故障切换",
            590,
            18,
            185,
            26,
            ID_EDITOR_AUTO,
            WS_CHILD | WS_VISIBLE | BS_AUTOCHECKBOX as u32 | WS_TABSTOP,
            instance,
            font,
        );
        editor_control(
            hwnd,
            "BUTTON",
            "路由范围",
            20,
            82,
            760,
            108,
            0,
            WS_CHILD | WS_VISIBLE | BS_GROUPBOX as u32,
            instance,
            font,
        );
        editor_control(
            hwnd,
            "STATIC",
            "协议",
            38,
            110,
            80,
            20,
            0,
            WS_CHILD | WS_VISIBLE,
            instance,
            font,
        );
        editor_control(
            hwnd,
            "COMBOBOX",
            "",
            38,
            131,
            160,
            180,
            ID_EDITOR_PROTOCOL,
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | CBS_DROPDOWNLIST as u32 | WS_VSCROLL,
            instance,
            font,
        );
        editor_control(
            hwnd,
            "STATIC",
            "源 Provider",
            218,
            110,
            120,
            20,
            0,
            WS_CHILD | WS_VISIBLE,
            instance,
            font,
        );
        editor_control(
            hwnd,
            "COMBOBOX",
            "",
            218,
            131,
            532,
            220,
            ID_EDITOR_SOURCE,
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | CBS_DROPDOWNLIST as u32 | WS_VSCROLL,
            instance,
            font,
        );
        editor_control(
            hwnd,
            "STATIC",
            "",
            218,
            161,
            532,
            18,
            ID_EDITOR_SOURCE_DETAIL,
            WS_CHILD | WS_VISIBLE,
            instance,
            font,
        );
        editor_control(
            hwnd,
            "BUTTON",
            "为此 Provider 使用自定义转移顺序",
            24,
            210,
            390,
            26,
            ID_EDITOR_CUSTOM,
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_AUTOCHECKBOX as u32,
            instance,
            font,
        );
        editor_control(
            hwnd,
            "BUTTON",
            "目标与优先级",
            20,
            246,
            760,
            276,
            0,
            WS_CHILD | WS_VISIBLE | BS_GROUPBOX as u32,
            instance,
            font,
        );
        editor_control(
            hwnd,
            "STATIC",
            "可选 Provider",
            36,
            272,
            300,
            20,
            0,
            WS_CHILD | WS_VISIBLE,
            instance,
            font,
        );
        editor_control(
            hwnd,
            "STATIC",
            "故障转移优先级",
            444,
            272,
            300,
            20,
            0,
            WS_CHILD | WS_VISIBLE,
            instance,
            font,
        );
        editor_control(
            hwnd,
            "LISTBOX",
            "",
            36,
            294,
            300,
            204,
            ID_EDITOR_AVAILABLE,
            WS_CHILD
                | WS_VISIBLE
                | WS_TABSTOP
                | WS_VSCROLL
                | LBS_NOTIFY as u32
                | LBS_NOINTEGRALHEIGHT as u32,
            instance,
            font,
        );
        editor_control(
            hwnd,
            "LISTBOX",
            "",
            444,
            294,
            320,
            204,
            ID_EDITOR_TARGETS,
            WS_CHILD
                | WS_VISIBLE
                | WS_TABSTOP
                | WS_VSCROLL
                | LBS_NOTIFY as u32
                | LBS_NOINTEGRALHEIGHT as u32,
            instance,
            font,
        );
        editor_control(
            hwnd,
            "BUTTON",
            "添加  >",
            354,
            330,
            72,
            30,
            ID_EDITOR_ADD,
            WS_CHILD | WS_VISIBLE | WS_TABSTOP,
            instance,
            font,
        );
        editor_control(
            hwnd,
            "BUTTON",
            "<  移除",
            354,
            368,
            72,
            30,
            ID_EDITOR_REMOVE,
            WS_CHILD | WS_VISIBLE | WS_TABSTOP,
            instance,
            font,
        );
        editor_control(
            hwnd,
            "BUTTON",
            "上移",
            354,
            422,
            72,
            30,
            ID_EDITOR_UP,
            WS_CHILD | WS_VISIBLE | WS_TABSTOP,
            instance,
            font,
        );
        editor_control(
            hwnd,
            "BUTTON",
            "下移",
            354,
            460,
            72,
            30,
            ID_EDITOR_DOWN,
            WS_CHILD | WS_VISIBLE | WS_TABSTOP,
            instance,
            font,
        );
        editor_control(
            hwnd,
            "STATIC",
            "",
            24,
            534,
            750,
            22,
            ID_EDITOR_STATUS,
            WS_CHILD | WS_VISIBLE,
            instance,
            font,
        );
        editor_control(
            hwnd,
            "STATIC",
            "",
            20,
            562,
            760,
            2,
            0,
            WS_CHILD | WS_VISIBLE | SS_ETCHEDHORZ_STYLE,
            instance,
            font,
        );
        editor_control(
            hwnd,
            "BUTTON",
            "保存并应用",
            552,
            578,
            120,
            34,
            ID_EDITOR_SAVE,
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_DEFPUSHBUTTON as u32,
            instance,
            font,
        );
        editor_control(
            hwnd,
            "BUTTON",
            "取消",
            684,
            578,
            90,
            34,
            ID_EDITOR_CANCEL,
            WS_CHILD | WS_VISIBLE | WS_TABSTOP,
            instance,
            font,
        );
        SendMessageW(
            GetDlgItem(hwnd, ID_EDITOR_AUTO as i32),
            BM_SETCHECK,
            if self.auto_failover {
                BST_CHECKED as usize
            } else {
                BST_UNCHECKED as usize
            },
            0,
        );
        SendMessageW(
            GetDlgItem(hwnd, ID_EDITOR_PROTOCOL as i32),
            CB_ADDSTRING,
            0,
            wide("Codex").as_ptr() as LPARAM,
        );
        SendMessageW(
            GetDlgItem(hwnd, ID_EDITOR_PROTOCOL as i32),
            CB_ADDSTRING,
            0,
            wide("Claude").as_ptr() as LPARAM,
        );
        SendMessageW(
            GetDlgItem(hwnd, ID_EDITOR_PROTOCOL as i32),
            CB_SETCURSEL,
            if self.protocol == Protocol::OpenAi {
                0
            } else {
                1
            },
            0,
        );
        self.refresh_sources(hwnd);
    }
}
