use super::*;

#[allow(unsafe_op_in_unsafe_fn)]
impl FailoverEditor {
    pub(super) unsafe fn add_selected(&mut self, hwnd: HWND) {
        let Some(source) = self.source_provider.clone() else {
            return;
        };
        if !self.policy.rules(self.protocol).contains_key(&source) {
            return;
        }
        let index = SendMessageW(
            GetDlgItem(hwnd, ID_EDITOR_AVAILABLE as i32),
            LB_GETCURSEL,
            0,
            0,
        );
        if index < 0 {
            return;
        }
        let Some(provider) = self.available.get(index as usize).cloned() else {
            return;
        };
        let selected = {
            let targets = self
                .policy
                .rules_mut(self.protocol)
                .entry(source)
                .or_default();
            targets.push(provider);
            targets.len() - 1
        };
        self.dirty = true;
        self.refresh_targets(hwnd);
        SendMessageW(
            GetDlgItem(hwnd, ID_EDITOR_TARGETS as i32),
            LB_SETCURSEL,
            selected,
            0,
        );
        self.update_action_buttons(hwnd);
    }

    pub(super) unsafe fn remove_selected(&mut self, hwnd: HWND) {
        let Some(source) = self.source_provider.clone() else {
            return;
        };
        let index = SendMessageW(
            GetDlgItem(hwnd, ID_EDITOR_TARGETS as i32),
            LB_GETCURSEL,
            0,
            0,
        );
        if index < 0 {
            return;
        }
        let next = self
            .policy
            .rules_mut(self.protocol)
            .get_mut(&source)
            .and_then(|targets| {
                targets.remove(index as usize);
                (!targets.is_empty()).then_some((index as usize).min(targets.len() - 1))
            });
        self.dirty = true;
        self.refresh_targets(hwnd);
        if let Some(next) = next {
            SendMessageW(
                GetDlgItem(hwnd, ID_EDITOR_TARGETS as i32),
                LB_SETCURSEL,
                next,
                0,
            );
            self.update_action_buttons(hwnd);
        }
    }

    pub(super) unsafe fn move_selected(&mut self, hwnd: HWND, direction: isize) {
        let Some(source) = self.source_provider.clone() else {
            return;
        };
        let index = SendMessageW(
            GetDlgItem(hwnd, ID_EDITOR_TARGETS as i32),
            LB_GETCURSEL,
            0,
            0,
        );
        if index < 0 {
            return;
        }
        let Some(targets) = self.policy.rules_mut(self.protocol).get_mut(&source) else {
            return;
        };
        let Some(next) = move_target(targets, index as usize, direction) else {
            return;
        };
        self.dirty = true;
        self.refresh_targets(hwnd);
        SendMessageW(
            GetDlgItem(hwnd, ID_EDITOR_TARGETS as i32),
            LB_SETCURSEL,
            next,
            0,
        );
        self.update_action_buttons(hwnd);
    }

    pub(super) unsafe fn update_action_buttons(&self, hwnd: HWND) {
        let custom = self
            .source_provider
            .as_ref()
            .is_some_and(|source| self.policy.rules(self.protocol).contains_key(source));
        let available = GetDlgItem(hwnd, ID_EDITOR_AVAILABLE as i32);
        let targets = GetDlgItem(hwnd, ID_EDITOR_TARGETS as i32);
        let available_selected = SendMessageW(available, LB_GETCURSEL, 0, 0);
        let target_selected = SendMessageW(targets, LB_GETCURSEL, 0, 0);
        let target_count = SendMessageW(targets, LB_GETCOUNT, 0, 0);
        EnableWindow(
            GetDlgItem(hwnd, ID_EDITOR_ADD as i32),
            (custom && available_selected >= 0) as i32,
        );
        EnableWindow(
            GetDlgItem(hwnd, ID_EDITOR_REMOVE as i32),
            (custom && target_selected >= 0) as i32,
        );
        EnableWindow(
            GetDlgItem(hwnd, ID_EDITOR_UP as i32),
            (custom && target_selected > 0) as i32,
        );
        EnableWindow(
            GetDlgItem(hwnd, ID_EDITOR_DOWN as i32),
            (custom && target_selected >= 0 && target_selected + 1 < target_count) as i32,
        );
    }
}
