use super::*;

pub(super) fn show_takeover_preview(hwnd: HWND, app: &AppState) {
    let config = app.inner.lock().unwrap().config.clone();
    let preferred_openai = app.active_url();
    let preferred_anthropic = app.active_anthropic_url();
    let plan = match prepare_takeover(
        &config,
        preferred_openai.as_deref(),
        preferred_anthropic.as_deref(),
    ) {
        Ok(plan) => plan,
        Err(error) => {
            notify(hwnd, "配置接管预览失败", &error.to_string());
            return;
        }
    };
    if plan.preview.changes.is_empty() {
        notify(hwnd, "配置接管", "Codex 和 Claude 已由 HeadroomRoute 管理");
        return;
    }
    let confirmation_token = plan.preview.confirmation_token.clone();
    let preview = limit_ui_text(&takeover_plan_text(&plan), 12_000);
    let confirmed = unsafe {
        MessageBoxW(
            hwnd,
            wide(&preview).as_ptr(),
            wide("查看配置接管").as_ptr(),
            MB_YESNO | MB_ICONWARNING,
        ) == IDYES
    };
    if !confirmed {
        return;
    }
    let result = {
        let _config_guard = app.config_write_guard();
        let backup = create_config_backup(&config);
        match backup {
            Ok(backup) => apply_takeover_plan(plan, &confirmation_token).map(|()| Some(backup.id)),
            Err(error) => Err(error),
        }
    };
    match result {
        Ok(backup_id) => notify(
            hwnd,
            "配置接管 applied",
            &format!(
                "Codex/Claude 现在使用本地路由代理。备份：{}",
                backup_id.unwrap_or_else(|| "无".into())
            ),
        ),
        Err(error) => notify(hwnd, "配置接管 failed", &error.to_string()),
    }
}

pub(super) fn create_backup_from_tray(hwnd: HWND, app: &AppState) {
    let config = app.inner.lock().unwrap().config.clone();
    let result = {
        let _config_guard = app.config_write_guard();
        create_config_backup(&config)
    };
    match result {
        Ok(backup) => notify(hwnd, "配置备份已创建", &backup_summary(&backup)),
        Err(error) => notify(hwnd, "配置备份失败", &error.to_string()),
    }
}

pub(super) fn restore_backup_from_tray(hwnd: HWND, app: &AppState) {
    let config = app.inner.lock().unwrap().config.clone();
    let backups = match list_config_backups(&config) {
        Ok(backups) => backups,
        Err(error) => {
            notify(hwnd, "无法列出配置备份", &error.to_string());
            return;
        }
    };
    if backups.is_empty() {
        notify(hwnd, "没有配置备份", "恢复前请先创建备份");
        return;
    }
    let root = config.state_dir.join("backups");
    let Some(manifest) = choose_file(
        hwnd,
        false,
        "选择 HeadroomRoute 备份清单",
        "备份清单\0manifest.json\0所有文件\0*.*\0\0",
        "json",
        Some(&root),
    ) else {
        return;
    };
    let backup_id = match selected_backup_id(&config, &manifest, &backups) {
        Ok(id) => id,
        Err(error) => {
            notify(hwnd, "配置备份无效", &error.to_string());
            return;
        }
    };
    let Some(descriptor) = backups.iter().find(|backup| backup.id == backup_id) else {
        notify(hwnd, "未找到配置备份", "请选择列表中的备份");
        return;
    };
    let prompt = limit_ui_text(
        &format!(
            "恢复备份 {}（创建于 {}）？\n\n{}\n\n当前配置文件可能会被替换。",
            descriptor.id,
            descriptor.created_at,
            backup_summary(descriptor)
        ),
        8_000,
    );
    let confirmed = unsafe {
        MessageBoxW(
            hwnd,
            wide(&prompt).as_ptr(),
            wide("确认恢复配置").as_ptr(),
            MB_YESNO | MB_ICONWARNING,
        ) == IDYES
    };
    if !confirmed {
        return;
    }
    let result = {
        let _config_guard = app.config_write_guard();
        restore_config_backup(&config, &backup_id)
    };
    match result {
        Ok(restored) => match reload_config_after_external_write(app) {
            Ok(()) => notify(hwnd, "配置已恢复", &backup_summary(&restored)),
            Err(error) => notify(hwnd, "备份已恢复，但重新加载失败", &error.to_string()),
        },
        Err(error) => notify(hwnd, "配置恢复失败", &error.to_string()),
    }
}

pub(super) fn export_portable_from_tray(hwnd: HWND, app: &AppState) {
    let config = app.inner.lock().unwrap().config.clone();
    let default_path = config.state_dir.join("HeadroomRoute-portable.json");
    let Some(destination) = choose_file(
        hwnd,
        true,
        "导出 HeadroomRoute 可移植配置",
        "可移植配置\0*.json\0所有文件\0*.*\0\0",
        "json",
        default_path.parent(),
    ) else {
        return;
    };
    match export_portable_config(&config, &destination) {
        Ok(()) => notify(hwnd, "可移植配置已导出", &destination.display().to_string()),
        Err(error) => notify(hwnd, "可移植配置导出失败", &error.to_string()),
    }
}

pub(super) fn import_portable_from_tray(hwnd: HWND, app: &AppState) {
    let config = app.inner.lock().unwrap().config.clone();
    let Some(source) = choose_file(
        hwnd,
        false,
        "导入 HeadroomRoute 可移植配置",
        "可移植配置\0*.json\0所有文件\0*.*\0\0",
        "json",
        Some(&config.state_dir),
    ) else {
        return;
    };
    let bytes = match fs::read(&source) {
        Ok(bytes) => bytes,
        Err(error) => {
            notify(hwnd, "可移植配置导入失败", &error.to_string());
            return;
        }
    };
    let updated = match decode_portable_config(&bytes, &config) {
        Ok(updated) => updated,
        Err(error) => {
            notify(hwnd, "可移植配置已拒绝", &error.to_string());
            return;
        }
    };
    let prompt = limit_ui_text(
        &format!(
            "导入这些非敏感配置？\n\n{}\n\n现有配置将先备份。",
            portable_change_summary(&config, &updated)
        ),
        8_000,
    );
    let confirmed = unsafe {
        MessageBoxW(
            hwnd,
            wide(&prompt).as_ptr(),
            wide("确认导入可移植配置").as_ptr(),
            MB_YESNO | MB_ICONQUESTION,
        ) == IDYES
    };
    if !confirmed {
        return;
    }
    let destination = config.state_dir.join("config.json");
    let result = {
        let _config_guard = app.config_write_guard();
        create_config_backup(&config).and_then(|backup| {
            import_portable_config(&source, &destination, &config).map(|_| backup)
        })
    };
    match result {
        Ok(backup) => match reload_config_after_external_write(app) {
            Ok(()) => notify(hwnd, "可移植配置已导入", &format!("备份：{}", backup.id)),
            Err(error) => notify(hwnd, "可移植配置已导入，但重新加载失败", &error.to_string()),
        },
        Err(error) => notify(hwnd, "可移植配置导入失败", &error.to_string()),
    }
}

pub(super) fn create_diagnostic_zip_from_tray(hwnd: HWND, app: &AppState) {
    let config = app.inner.lock().unwrap().config.clone();
    let default_path = config.state_dir.join("HeadroomRoute-diagnostic.zip");
    let Some(destination) = choose_file(
        hwnd,
        true,
        "创建脱敏 HeadroomRoute 诊断 ZIP",
        "ZIP 压缩包\0*.zip\0所有文件\0*.*\0\0",
        "zip",
        default_path.parent(),
    ) else {
        return;
    };
    let report = app.diagnostic_text();
    match create_diagnostic_bundle(&config, &destination, Some(&report)) {
        Ok(descriptor) => notify(
            hwnd,
            "诊断 ZIP 已创建",
            &format!(
                "已写入 {} 项到 {}",
                descriptor.entries.len(),
                destination.display()
            ),
        ),
        Err(error) => notify(hwnd, "诊断 ZIP 创建失败", &error.to_string()),
    }
}

fn takeover_plan_text(plan: &TakeoverPlan) -> String {
    let mut text = format!(
        "Files to change: {}\nPreview token: {}\n",
        plan.preview.changes.len(),
        plan.preview.confirmation_token
    );
    for change in &plan.preview.changes {
        text.push_str(&format!("\n{:?}: {}\n", change.kind, change.path));
        for field in &change.fields {
            text.push_str(&format!(
                "  {}: {} -> {}\n",
                field.field,
                preview_value(field.before.as_ref()),
                preview_value(field.after.as_ref())
            ));
        }
    }
    text.push_str("\n选择“是”以创建备份并应用这份精确预览。");
    text
}

fn preview_value(value: Option<&serde_json::Value>) -> String {
    value.map_or_else(
        || "<missing>".into(),
        |value| serde_json::to_string(value).unwrap_or_else(|_| "<unavailable>".into()),
    )
}

fn backup_summary(backup: &BackupDescriptor) -> String {
    let present = backup.files.iter().filter(|file| file.present).count();
    format!(
        "备份 {} 包含 {} 个受跟踪文件（当前存在 {} 个）",
        backup.id,
        backup.files.len(),
        present
    )
}

fn portable_change_summary(
    current: &crate::model::AppConfig,
    updated: &crate::model::AppConfig,
) -> String {
    let mut changes = Vec::new();
    if current.agent_port != updated.agent_port {
        changes.push(format!(
            "agent_port: {} -> {}",
            current.agent_port, updated.agent_port
        ));
    }
    if current.headroom_port != updated.headroom_port {
        changes.push(format!(
            "headroom_port: {} -> {}",
            current.headroom_port, updated.headroom_port
        ));
    }
    if current.enable_codex != updated.enable_codex {
        changes.push(format!(
            "enable_codex: {} -> {}",
            current.enable_codex, updated.enable_codex
        ));
    }
    if current.enable_claude != updated.enable_claude {
        changes.push(format!(
            "enable_claude: {} -> {}",
            current.enable_claude, updated.enable_claude
        ));
    }
    if current.auto_failover != updated.auto_failover {
        changes.push(format!(
            "auto_failover: {} -> {}",
            current.auto_failover, updated.auto_failover
        ));
    }
    if current.failover_policy != updated.failover_policy {
        changes.push("failover_policy".into());
    }
    if current.manage_headroom != updated.manage_headroom {
        changes.push(format!(
            "manage_headroom: {} -> {}",
            current.manage_headroom, updated.manage_headroom
        ));
    }
    if current.start_with_windows != updated.start_with_windows {
        changes.push(format!(
            "start_with_windows: {} -> {}",
            current.start_with_windows, updated.start_with_windows
        ));
    }
    if current.no_subscription_tracking != updated.no_subscription_tracking {
        changes.push(format!(
            "no_subscription_tracking: {} -> {}",
            current.no_subscription_tracking, updated.no_subscription_tracking
        ));
    }
    if current.use_system_proxy != updated.use_system_proxy {
        changes.push(format!(
            "use_system_proxy: {} -> {}",
            current.use_system_proxy, updated.use_system_proxy
        ));
    }
    if current.bypass_headroom != updated.bypass_headroom {
        changes.push(format!(
            "bypass_headroom: {} -> {}",
            current.bypass_headroom, updated.bypass_headroom
        ));
    }
    if current.manage_codex != updated.manage_codex {
        changes.push(format!(
            "manage_codex: {} -> {}",
            current.manage_codex, updated.manage_codex
        ));
    }
    if current.manage_claude != updated.manage_claude {
        changes.push(format!(
            "manage_claude: {} -> {}",
            current.manage_claude, updated.manage_claude
        ));
    }
    if current.direct_codex != updated.direct_codex {
        changes.push(format!(
            "direct_codex: {} -> {}",
            current.direct_codex, updated.direct_codex
        ));
    }
    if current.direct_claude != updated.direct_claude {
        changes.push(format!(
            "direct_claude: {} -> {}",
            current.direct_claude, updated.direct_claude
        ));
    }
    if current.auto_check_updates != updated.auto_check_updates {
        changes.push(format!(
            "auto_check_updates: {} -> {}",
            current.auto_check_updates, updated.auto_check_updates
        ));
    }
    if current.show_api_key_on_hover != updated.show_api_key_on_hover {
        changes.push(format!(
            "show_api_key_on_hover: {} -> {}",
            current.show_api_key_on_hover, updated.show_api_key_on_hover
        ));
    }
    if current.routing_strategy != updated.routing_strategy {
        changes.push("routing_strategy".into());
    }
    if changes.is_empty() {
        "No effective setting changes".into()
    } else {
        changes.join("\n")
    }
}

fn reload_config_after_external_write(app: &AppState) -> anyhow::Result<()> {
    let path = app
        .inner
        .lock()
        .unwrap()
        .config
        .state_dir
        .join("config.json");
    let updated = config::load_or_create(&path)?;
    app.inner.lock().unwrap().config = updated;
    app.refresh_routes();
    Ok(())
}

fn selected_backup_id(
    config: &crate::model::AppConfig,
    manifest: &Path,
    backups: &[BackupDescriptor],
) -> anyhow::Result<String> {
    let root = config.state_dir.join("backups").canonicalize()?;
    let manifest = manifest.canonicalize()?;
    if manifest.file_name().and_then(|name| name.to_str()) != Some("manifest.json")
        || manifest.parent().and_then(Path::parent) != Some(root.as_path())
    {
        anyhow::bail!("选择的文件不在 HeadroomRoute 备份目录中");
    }
    let id = manifest
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("无法读取备份 ID"))?;
    if !backups.iter().any(|backup| backup.id == id) {
        anyhow::bail!("备份清单未通过校验或已不存在");
    }
    Ok(id.to_owned())
}

fn choose_file(
    parent: HWND,
    save: bool,
    title: &str,
    filter: &str,
    default_extension: &str,
    initial_dir: Option<&Path>,
) -> Option<PathBuf> {
    let mut buffer = vec![0u16; 32 * 1024];
    let filter = wide(filter);
    let title = wide(title);
    let default_extension = wide(default_extension);
    let initial = initial_dir.map(|path| wide(&path.to_string_lossy()));
    let mut dialog = OPENFILENAMEW {
        lStructSize: size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: parent,
        lpstrFilter: filter.as_ptr(),
        nFilterIndex: 1,
        lpstrFile: buffer.as_mut_ptr(),
        nMaxFile: buffer.len() as u32,
        lpstrInitialDir: initial.as_ref().map_or(ptr::null(), |value| value.as_ptr()),
        lpstrTitle: title.as_ptr(),
        lpstrDefExt: default_extension.as_ptr(),
        Flags: if save {
            OFN_NOCHANGEDIR | OFN_PATHMUSTEXIST | OFN_OVERWRITEPROMPT
        } else {
            OFN_FILEMUSTEXIST | OFN_NOCHANGEDIR | OFN_PATHMUSTEXIST
        },
        ..OPENFILENAMEW::default()
    };
    let selected = unsafe {
        if save {
            GetSaveFileNameW(&mut dialog)
        } else {
            GetOpenFileNameW(&mut dialog)
        }
    };
    if selected == 0 {
        return None;
    }
    let length = buffer
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(buffer.len());
    (length > 0).then(|| PathBuf::from(String::from_utf16_lossy(&buffer[..length])))
}

fn limit_ui_text(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    text.chars()
        .take(limit.saturating_sub(32))
        .collect::<String>()
        + "\n... output truncated ..."
}
