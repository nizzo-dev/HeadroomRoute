#![cfg(windows)]
use crate::{config, state::AppState};
use std::{cell::Cell, ffi::c_void, mem::size_of, process::Command, ptr, sync::{Arc, OnceLock, atomic::Ordering}, thread};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
    System::{DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData}, LibraryLoader::GetModuleHandleW, Ole::CF_UNICODETEXT, Registry::{HKEY_CURRENT_USER, KEY_SET_VALUE, REG_SZ, RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegSetValueExW}},
    UI::{Shell::{NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW, Shell_NotifyIconW}, WindowsAndMessaging::*},
};

const WM_TRAY: u32 = WM_APP + 1;
const SS_CENTERIMAGE_STYLE: u32 = 0x0000_0200;
const ID_OPEN_STATUS: usize = 100;
const ID_SYNC: usize = 101;
const ID_CHECK: usize = 102;
const ID_RESTART: usize = 103;
const ID_STARTUP: usize = 105;
const ID_DIAG: usize = 106;
const ID_CONFIG: usize = 107;
const ID_LOGS: usize = 108;
const ID_EXIT: usize = 109;
const ID_RESTORE: usize = 110;
const ID_REPAIR_RUNTIME: usize = 111;
const ID_UNINSTALL: usize = 112;
const ID_ROUTE_BASE: usize = 1000;
static APP: OnceLock<Arc<AppState>> = OnceLock::new();
thread_local! { static URL_POPUP: Cell<HWND> = const { Cell::new(ptr::null_mut()) }; }

pub fn run(app: Arc<AppState>) -> anyhow::Result<()> {
    let _ = APP.set(app);
    unsafe {
        let instance = GetModuleHandleW(ptr::null());
        let class_name = wide("HeadroomRouteTrayWindow");
        let class = WNDCLASSW { lpfnWndProc: Some(window_proc), hInstance: instance, lpszClassName: class_name.as_ptr(), ..std::mem::zeroed() };
        if RegisterClassW(&class) == 0 { anyhow::bail!("无法注册托盘窗口"); }
        let hwnd = CreateWindowExW(0, class_name.as_ptr(), wide("Headroom Route").as_ptr(), WS_OVERLAPPED, 0,0,0,0, ptr::null_mut(), ptr::null_mut(), instance, ptr::null());
        if hwnd.is_null() { anyhow::bail!("无法创建托盘窗口"); }
        add_icon(hwnd);
        SetTimer(hwnd, 1, 500, None);
        let mut message: MSG = std::mem::zeroed();
        while GetMessageW(&mut message, ptr::null_mut(), 0, 0) > 0 { TranslateMessage(&message); DispatchMessageW(&message); }
        remove_icon(hwnd);
    }
    Ok(())
}

unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match message {
        WM_TRAY if lparam as u32 == WM_RBUTTONUP || lparam as u32 == WM_CONTEXTMENU => { unsafe { show_menu(hwnd) }; 0 }
        WM_TRAY if lparam as u32 == WM_LBUTTONDBLCLK => { unsafe { show_status(hwnd) }; 0 }
        WM_COMMAND => { unsafe { handle_command(hwnd, wparam & 0xffff) }; 0 }
        WM_MENUSELECT => { unsafe { show_hovered_route_url(hwnd, wparam) }; 0 }
        WM_EXITMENULOOP => { unsafe { hide_route_url() }; 0 }
        WM_TIMER => {
            unsafe { update_icon(hwnd) };
            if let Some(app) = APP.get() {
                if let Some((ok, message)) = app.take_sync_result() {
                    if ok { notify(hwnd, "同步完成", &message); } else { notify(hwnd, "同步失败", &message); }
                }
                if let Some((ok, message)) = app.take_restart_result() {
                    if ok { notify(hwnd, "Headroom 重启完成", &message); } else { notify(hwnd, "Headroom 重启失败", &message); }
                }
            }
            0
        }
        WM_DESTROY => { unsafe { destroy_route_url() }; if let Some(app)=APP.get(){app.stop.store(true, Ordering::Relaxed);} unsafe { PostQuitMessage(0) }; 0 }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

unsafe fn show_menu(hwnd: HWND) {
    let Some(app) = APP.get() else { return };
    let snapshot = app.snapshot();
    let menu = unsafe { CreatePopupMenu() };
    let title = format!("Codex：{} · Claude：{} · {}", snapshot.active_name.as_deref().unwrap_or("未配置"), snapshot.active_anthropic_name.as_deref().unwrap_or("未配置"), health_cn(snapshot.state));
    unsafe { AppendMenuW(menu, MF_STRING | MF_DISABLED, ID_OPEN_STATUS, wide(&title).as_ptr()) };
    unsafe { AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null()) };
    for (index, route) in snapshot.routes.iter().take(32).enumerate() {
        let selected = snapshot.active_url.as_deref() == Some(&route.base_url)
            || snapshot.active_anthropic_url.as_deref() == Some(&route.base_url);
        let flags = MF_STRING | if selected { MF_CHECKED } else { 0 };
        let text = format!("[{}] {}  {} · {}ms", route.protocol.label(), route.name, route.state.label(), route.latency_ms.map(|v|v.to_string()).unwrap_or_else(||"--".into()));
        unsafe { AppendMenuW(menu, flags, ID_ROUTE_BASE + index, wide(&text).as_ptr()) };
    }
    unsafe {
        AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
        AppendMenuW(menu, MF_STRING, ID_CHECK, wide("立即检查").as_ptr());
        let (sync_flags, sync_text) = if app.sync_in_progress.load(Ordering::Acquire) {
            (MF_STRING | MF_DISABLED, "正在同步 Codex + Claude...")
        } else if snapshot.sync_status == "同步完成" {
            (MF_STRING, "同步配置（上次已完成）")
        } else {
            (MF_STRING, "同步 Codex + Claude / CC-Switch")
        };
        AppendMenuW(menu, sync_flags, ID_SYNC, wide(sync_text).as_ptr());
        let (restart_flags, restart_text) = if app.restart_in_progress.load(Ordering::Acquire) {
            (MF_STRING | MF_DISABLED, "正在重启 Headroom...")
        } else if snapshot.restart_status == "重启完成" {
            (MF_STRING, "重启 Headroom（上次已完成）")
        } else {
            (MF_STRING, "重启 Headroom")
        };
        AppendMenuW(menu, restart_flags, ID_RESTART, wide(restart_text).as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
        let startup = app.inner.lock().unwrap().config.start_with_windows;
        AppendMenuW(menu, MF_STRING | if startup { MF_CHECKED } else { 0 }, ID_STARTUP, wide("开机启动").as_ptr());
        AppendMenuW(menu, MF_STRING, ID_DIAG, wide("复制诊断报告").as_ptr());
        AppendMenuW(menu, MF_STRING, ID_CONFIG, wide("打开配置文件").as_ptr());
        AppendMenuW(menu, MF_STRING, ID_LOGS, wide("打开数据目录").as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
        AppendMenuW(menu, MF_STRING, ID_RESTORE, wide("恢复 Codex / Claude 配置").as_ptr());
        AppendMenuW(menu, MF_STRING, ID_REPAIR_RUNTIME, wide("修复 Headroom 运行环境").as_ptr());
        AppendMenuW(menu, MF_STRING, ID_UNINSTALL, wide("完全卸载并还原").as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
        AppendMenuW(menu, MF_STRING, ID_EXIT, wide("退出").as_ptr());
        let mut point = POINT::default(); GetCursorPos(&mut point); SetForegroundWindow(hwnd);
        TrackPopupMenu(menu, TPM_RIGHTBUTTON, point.x, point.y, 0, hwnd, ptr::null());
        DestroyMenu(menu);
    }
}

unsafe fn handle_command(hwnd: HWND, id: usize) {
    let Some(app) = APP.get() else { return };
    match id {
        ID_OPEN_STATUS => unsafe { show_status(hwnd) },
        ID_CHECK => app.force_probe.store(true, Ordering::Relaxed),
        ID_SYNC => {
            if !app.begin_sync() { notify(hwnd, "正在同步", "请等待当前同步完成"); return; }
            notify(hwnd, "同步中", "正在读取 CC-Switch 并更新 Codex / Claude Code");
            let app = app.clone();
            thread::spawn(move || {
                let cfg = app.inner.lock().unwrap().config.clone();
                let active_url = app.active_url();
                match config::sync_all(&cfg, active_url.as_deref()) {
                    Ok(_) => { app.refresh_routes(); let _ = app.write_status(); app.finish_sync(true, "Codex 与 Claude Code 配置同步完成".into()); }
                    Err(error) => app.finish_sync(false, error.to_string()),
                }
            });
        }
        ID_RESTART => {
            if !app.begin_restart() { notify(hwnd, "正在重启", "请等待当前 Headroom 重启完成"); return; }
            app.restart_headroom.store(true, Ordering::Release);
            notify(hwnd, "Headroom 重启中", "正在停止并重新启动 Headroom，请稍候");
        }
        ID_STARTUP => { let enabled={let mut state=app.inner.lock().unwrap();state.config.start_with_windows=!state.config.start_with_windows;let path=state.config.state_dir.join("config.json");let _=config::save(&path,&state.config);state.config.start_with_windows}; if let Err(e)=set_startup(enabled){notify(hwnd,"开机启动设置失败",&e.to_string())} }
        ID_DIAG => { let text=app.diagnostic_text(); if copy_clipboard(hwnd,&text).is_ok(){notify(hwnd,"诊断报告已复制","报告不包含 API Key")}; }
        ID_CONFIG => { let path=app.inner.lock().unwrap().config.state_dir.join("config.json"); let _=Command::new("notepad.exe").arg(path).spawn(); }
        ID_LOGS => { let path=app.inner.lock().unwrap().config.state_dir.clone(); let _=Command::new("explorer.exe").arg(path).spawn(); }
        ID_RESTORE => { *app.maintenance_action.lock().unwrap()=Some("restore".into()); unsafe { DestroyWindow(hwnd); } }
        ID_REPAIR_RUNTIME => {
            if unsafe { MessageBoxW(hwnd,wide("修复会停止 Headroom 并重新安装托管运行环境，是否继续？").as_ptr(),wide("修复 Headroom").as_ptr(),MB_YESNO|MB_ICONWARNING) } == IDYES {
                *app.maintenance_action.lock().unwrap()=Some("repair".into()); unsafe { DestroyWindow(hwnd); }
            }
        }
        ID_UNINSTALL => {
            if unsafe { MessageBoxW(hwnd,wide("将恢复 Codex/Claude 配置、删除托管运行环境并取消开机启动。是否完全卸载？").as_ptr(),wide("完全卸载 HeadroomRoute").as_ptr(),MB_YESNO|MB_ICONWARNING) } == IDYES {
                *app.maintenance_action.lock().unwrap()=Some("uninstall".into()); unsafe { DestroyWindow(hwnd); }
            }
        }
        ID_EXIT => unsafe { DestroyWindow(hwnd); },
        value if value >= ID_ROUTE_BASE => { if app.switch_index(value-ID_ROUTE_BASE,"托盘手动切换"){let _=app.write_status();} }
        _ => {}
    }
}

unsafe fn show_status(hwnd: HWND) {
    let Some(app)=APP.get() else{return}; let s=app.snapshot();
    let text=format!("Codex 上游：{}\r\nClaude 上游：{}\r\n路由状态：{}（Codex {} ms / Claude {} ms）\r\n同步状态：{}\r\n重启状态：{}\r\nHeadroom：{}\r\n路由策略：仅由 HeadroomRoute 选择\r\n路由数量：{}\r\n最近切换：{}\r\n最近错误：{}",s.active_name.as_deref().unwrap_or("未配置"),s.active_anthropic_name.as_deref().unwrap_or("未配置"),health_cn(s.state),s.latency_ms.map(|v|v.to_string()).unwrap_or_else(||"--".into()),s.anthropic_latency_ms.map(|v|v.to_string()).unwrap_or_else(||"--".into()),s.sync_status,s.restart_status,s.headroom_state,s.routes.len(),s.last_switch_reason.as_deref().unwrap_or("无"),s.last_error.as_deref().unwrap_or("无"));
    unsafe { MessageBoxW(hwnd,wide(&text).as_ptr(),wide("Headroom Route 状态").as_ptr(),MB_OK|MB_ICONINFORMATION) };
}

unsafe fn show_hovered_route_url(hwnd: HWND, wparam: WPARAM) {
    let id = wparam & 0xffff;
    let Some(route) = APP.get().and_then(|app| app.snapshot().routes.get(id.wrapping_sub(ID_ROUTE_BASE)).cloned()) else {
        unsafe { hide_route_url() };
        return;
    };
    URL_POPUP.with(|slot| unsafe {
        let mut popup = slot.get();
        if popup.is_null() {
            popup = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
                wide("STATIC").as_ptr(), ptr::null(), WS_POPUP | WS_BORDER | SS_CENTERIMAGE_STYLE,
                0, 0, 0, 0, hwnd, ptr::null_mut(), GetModuleHandleW(ptr::null()), ptr::null(),
            );
            slot.set(popup);
        }
        if popup.is_null() { return; }
        SetWindowTextW(popup, wide(&route.base_url).as_ptr());
        let mut point = POINT::default(); GetCursorPos(&mut point);
        let width = (route.base_url.chars().count() as i32 * 7 + 28).clamp(300, 720);
        SetWindowPos(popup, HWND_TOPMOST, point.x + 18, point.y + 18, width, 30, SWP_NOACTIVATE | SWP_SHOWWINDOW);
    });
}

unsafe fn hide_route_url() {
    URL_POPUP.with(|slot| { let popup=slot.get(); if !popup.is_null() { unsafe { ShowWindow(popup, SW_HIDE); } } });
}

unsafe fn destroy_route_url() {
    URL_POPUP.with(|slot| { let popup=slot.replace(ptr::null_mut()); if !popup.is_null() { unsafe { DestroyWindow(popup); } } });
}

unsafe fn add_icon(hwnd: HWND) { let mut data=notify_data(hwnd); unsafe { Shell_NotifyIconW(NIM_ADD,&mut data); DestroyIcon(data.hIcon); } }
unsafe fn remove_icon(hwnd: HWND) { let mut data=notify_data(hwnd); unsafe { Shell_NotifyIconW(NIM_DELETE,&mut data); DestroyIcon(data.hIcon); } }
unsafe fn update_icon(hwnd: HWND) { let mut data=notify_data(hwnd); unsafe { Shell_NotifyIconW(NIM_MODIFY,&mut data); DestroyIcon(data.hIcon); } }
fn notify_data(hwnd: HWND) -> NOTIFYICONDATAW {
    let (tip, health)=APP.get().map(|app|{let s=app.snapshot();let codex=s.active_name.as_deref().unwrap_or("未配置").chars().take(24).collect::<String>();let claude=s.active_anthropic_name.as_deref().unwrap_or("未配置").chars().take(24).collect::<String>();(format!("Headroom Route\r\nCodex：{codex}\r\nClaude：{claude}\r\n{} · {} · {}",health_cn(s.state),s.sync_status,s.restart_status),s.state)}).unwrap_or(("Headroom Route".into(),"unknown"));
    let mut data:NOTIFYICONDATAW=unsafe{std::mem::zeroed()}; data.cbSize=size_of::<NOTIFYICONDATAW>() as u32;data.hWnd=hwnd;data.uID=1;data.uFlags=NIF_MESSAGE|NIF_ICON|NIF_TIP;data.uCallbackMessage=WM_TRAY;data.hIcon=make_icon(health);
    let chars=wide(&tip);for (dst,src) in data.szTip.iter_mut().zip(chars){*dst=src;} data
}
fn make_icon(health:&str)->*mut c_void {
    let color=match health{"healthy"=>0x00_3c_b3_71u32,"degraded"=>0x00_00_a5_ff,"unavailable"=>0x00_43_43_dc,_=>0x00_80_80_80};
    let mut and_mask=[0xffu8;32];
    let mut xor=[0u8;1024];
    draw_icon_line(&mut and_mask,&mut xor,3,12,7,8,color);
    draw_icon_line(&mut and_mask,&mut xor,7,8,7,4,color);
    draw_icon_line(&mut and_mask,&mut xor,7,4,13,4,color);
    draw_icon_line(&mut and_mask,&mut xor,11,2,13,4,color);
    draw_icon_line(&mut and_mask,&mut xor,11,6,13,4,color);
    for (x,y) in [(3,12),(7,8),(7,4)] { draw_icon_node(&mut and_mask,&mut xor,x,y,color); }
    unsafe{windows_sys::Win32::UI::WindowsAndMessaging::CreateIcon(ptr::null_mut(),16,16,1,32,and_mask.as_ptr(),xor.as_ptr())}
}
fn set_icon_pixel(and_mask:&mut [u8;32],xor:&mut [u8;1024],x:i32,y:i32,color:u32){
    if !(0..16).contains(&x)||!(0..16).contains(&y){return}
    let row=(15-y) as usize;let x=x as usize;
    and_mask[row*2+x/8]&=!(0x80>>(x%8));
    let offset=(row*16+x)*4;xor[offset..offset+4].copy_from_slice(&color.to_le_bytes());
}
fn draw_icon_line(and_mask:&mut [u8;32],xor:&mut [u8;1024],mut x0:i32,mut y0:i32,x1:i32,y1:i32,color:u32){
    let dx=(x1-x0).abs();let sx=if x0<x1{1}else{-1};let dy=-(y1-y0).abs();let sy=if y0<y1{1}else{-1};let mut error=dx+dy;
    loop{set_icon_pixel(and_mask,xor,x0,y0,color);set_icon_pixel(and_mask,xor,x0,y0+1,color);if x0==x1&&y0==y1{break}let twice=2*error;if twice>=dy{error+=dy;x0+=sx}if twice<=dx{error+=dx;y0+=sy}}
}
fn draw_icon_node(and_mask:&mut [u8;32],xor:&mut [u8;1024],x:i32,y:i32,color:u32){
    for (dx,dy) in [(0,-2),(-1,-1),(0,-1),(1,-1),(-2,0),(-1,0),(0,0),(1,0),(2,0),(-1,1),(0,1),(1,1),(0,2)]{set_icon_pixel(and_mask,xor,x+dx,y+dy,0x00_ff_ff_ff)}
    set_icon_pixel(and_mask,xor,x,y,color);
}
fn notify(hwnd:HWND,title:&str,message:&str){let mut data=notify_data(hwnd);data.uFlags|=windows_sys::Win32::UI::Shell::NIF_INFO;for(d,s)in data.szInfoTitle.iter_mut().zip(wide(title)){*d=s;}for(d,s)in data.szInfo.iter_mut().zip(wide(message)){*d=s;}unsafe{Shell_NotifyIconW(NIM_MODIFY,&mut data);DestroyIcon(data.hIcon);};}

fn set_startup(enabled:bool)->anyhow::Result<()> { unsafe { let mut key=ptr::null_mut();let sub=wide(r"Software\Microsoft\Windows\CurrentVersion\Run");if RegCreateKeyExW(HKEY_CURRENT_USER,sub.as_ptr(),0,ptr::null_mut(),0,KEY_SET_VALUE,ptr::null(),&mut key,ptr::null_mut())!=0{anyhow::bail!("无法打开启动项注册表")};let name=wide("HeadroomRoute");let result=if enabled{let exe=std::env::current_exe()?;let value=wide(&format!("\"{}\"",exe.display()));RegSetValueExW(key,name.as_ptr(),0,REG_SZ,value.as_ptr() as *const u8,(value.len()*2)as u32)}else{RegDeleteValueW(key,name.as_ptr())};RegCloseKey(key);if result!=0&&enabled{anyhow::bail!("注册表写入失败: {result}")};Ok(()) } }
fn copy_clipboard(hwnd:HWND,text:&str)->anyhow::Result<()> { unsafe { if OpenClipboard(hwnd)==0{anyhow::bail!("无法打开剪贴板")};EmptyClipboard();let value=wide(text);let bytes=value.len()*2;let memory=windows_sys::Win32::System::Memory::GlobalAlloc(windows_sys::Win32::System::Memory::GMEM_MOVEABLE,bytes);if memory.is_null(){CloseClipboard();anyhow::bail!("内存分配失败")};let target=windows_sys::Win32::System::Memory::GlobalLock(memory) as *mut u16;ptr::copy_nonoverlapping(value.as_ptr(),target,value.len());windows_sys::Win32::System::Memory::GlobalUnlock(memory);SetClipboardData(CF_UNICODETEXT.into(),memory as _);CloseClipboard();Ok(()) } }
fn health_cn(state:&str)->&'static str{match state{"healthy"=>"健康","degraded"=>"降级","unavailable"=>"不可用",_=>"检测中"}}
fn wide(value:&str)->Vec<u16>{value.encode_utf16().chain(Some(0)).collect()}
