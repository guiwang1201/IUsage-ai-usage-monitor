//! Local configuration only. This window never contacts a usage endpoint.
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use windows_sys::Win32::Security::Cryptography::*;
use windows_sys::Win32::UI::Controls::{EM_SETLIMITTEXT, EM_SETPASSWORDCHAR};
use crate::providers::web::Session;

static OPEN: AtomicBool = AtomicBool::new(false);
const KEY: i32 = 101;
const ORG: i32 = 102;
const SHOW: i32 = 103;
const ADVANCED: i32 = 104;
const HELP: i32 = 105;
const BROWSER: i32 = 106;
const SAVE: i32 = 107;
const CLEAR: i32 = 108;
const CANCEL: i32 = 109;
const NOTE: i32 = 110;
const ORG_LABEL: i32 = 111;
const USE_CODE: i32 = 112;

fn path() -> PathBuf { app_data_dir().join("IUsage").join("claude-web.dpapi") }

fn crypt(bytes: &[u8], encrypt: bool) -> Result<Vec<u8>, String> {
    let input = CRYPT_INTEGER_BLOB { cbData: bytes.len() as u32, pbData: bytes.as_ptr() as *mut u8 };
    let mut output: CRYPT_INTEGER_BLOB = unsafe { std::mem::zeroed() };
    unsafe {
        let ok = if encrypt { CryptProtectData(&input, null(), null(), null(), null(), CRYPTPROTECT_UI_FORBIDDEN, &mut output) }
            else { CryptUnprotectData(&input, null_mut(), null(), null(), null(), CRYPTPROTECT_UI_FORBIDDEN, &mut output) };
        if ok == 0 { return Err("Windows 无法保护或读取登录信息，请重新配置。".into()) }
        let result = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        LocalFree(output.pbData as *mut c_void);
        Ok(result)
    }
}

fn encode(session: &Session) -> Result<String, String> {
    // Validated values contain no newline; organization is an ASCII identifier.
    let plain = format!("{}\n{}", session.key, session.organization.as_deref().unwrap_or(""));
    Ok(crypt(plain.as_bytes(), true)?.iter().map(|b| format!("{b:02x}")).collect())
}

fn decode(encoded: &str) -> Result<Session, String> {
    if encoded.len() > 65536 || encoded.len() % 2 != 0 || !encoded.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("保存的网页登录信息损坏，请重新配置。".into());
    }
    let bytes = (0..encoded.len()).step_by(2).map(|i| u8::from_str_radix(&encoded[i..i+2], 16).unwrap()).collect::<Vec<_>>();
    let plain = String::from_utf8(crypt(&bytes, false)?).map_err(|_| "网页登录信息格式无效")?;
    let (key, org) = plain.split_once('\n').ok_or("网页登录信息格式无效")?;
    Session::from_input(key, org).map_err(|_| "网页登录信息格式无效".into())
}

pub fn load() -> Result<Session, String> {
    decode(&crate::json::read_small_file(&path()).map_err(|_| "请先配置 Claude 网页登录信息。")?)
}

pub fn selected(settings: &Settings) -> Result<Option<Session>, FetchError> {
    match settings.claude_web {
        Some(true) => load().map(Some).map_err(|_| FetchError::WebCredentials),
        Some(false) => Ok(None),
        None => Session::from_env(),
    }
}

pub fn is_web(settings: &Settings) -> bool {
    settings.claude_web.unwrap_or_else(|| std::env::var_os("IUSAGE_CLAUDE_SESSION_KEY").is_some())
}

struct Dialog { owner: usize, waiting: String, font: HFONT, load_saved: bool, expired: bool }

unsafe fn control(hwnd: HWND, class: &str, label: &str, id: i32, x: i32, y: i32, w: i32, h: i32, style: u32) -> HWND {
    let scale = GetDpiForWindow(hwnd) as f32 / 96.0;
    let p = |n: i32| (n as f32 * scale).round() as i32;
    CreateWindowExW(0, wide(class).as_ptr(), wide(label).as_ptr(), WS_CHILD | WS_VISIBLE | style,
        p(x), p(y), p(w), p(h), hwnd, id as usize as HMENU, GetModuleHandleW(null()), null())
}

unsafe fn value(hwnd: HWND, id: i32) -> String {
    let edit = GetDlgItem(hwnd, id);
    let mut text = vec![0u16; GetWindowTextLengthW(edit).max(0) as usize + 1];
    let count = GetWindowTextW(edit, text.as_mut_ptr(), text.len() as i32);
    String::from_utf16_lossy(&text[..count as usize])
}

unsafe fn note(hwnd: HWND, text: &str) { SetWindowTextW(GetDlgItem(hwnd, NOTE), wide(text).as_ptr()); }

unsafe extern "system" fn procedure(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_NCCREATE {
        let create = &*(lp as *const CREATESTRUCTW);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
    }
    let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Dialog;
    match msg {
        WM_CREATE => {
            let font = CreateFontW(-((GetDpiForWindow(hwnd) * 14 / 96) as i32), 0, 0, 0, 400, 0, 0, 0, 1, 0, 0, 4, 0, wide("Segoe UI").as_ptr());
            (*state).font = font;
            let fields = [
                control(hwnd,"STATIC","网页登录：无需安装 Claude Code。",0,20,16,460,24,0),
                control(hwnd,"STATIC","登录信息（sessionKey）",0,20,48,460,22,0),
                control(hwnd,"EDIT","",KEY,20,74,460,28,WS_BORDER | WS_TABSTOP | ES_PASSWORD as u32 | ES_AUTOHSCROLL as u32),
                control(hwnd,"BUTTON","显示登录信息",SHOW,20,110,160,24,WS_TABSTOP | BS_AUTOCHECKBOX as u32),
                control(hwnd,"BUTTON","如何获取？",HELP,260,108,100,28,WS_TABSTOP),
                control(hwnd,"BUTTON","打开网页",BROWSER,365,108,115,28,WS_TABSTOP),
                control(hwnd,"BUTTON","高级设置：组织 ID（通常留空）",ADVANCED,20,148,420,24,WS_TABSTOP | BS_AUTOCHECKBOX as u32),
                control(hwnd,"STATIC","组织 ID：多个组织时填写对应 UUID",ORG_LABEL,20,178,460,22,0),
                control(hwnd,"EDIT","",ORG,20,204,460,28,WS_BORDER | WS_TABSTOP | ES_AUTOHSCROLL as u32),
                control(hwnd,"STATIC","",NOTE,20,245,460,64,0),
                control(hwnd,"BUTTON","清除登录信息",CLEAR,20,325,130,32,WS_TABSTOP),
                control(hwnd,"BUTTON","取消",CANCEL,260,325,90,32,WS_TABSTOP),
                control(hwnd,"BUTTON","保存并使用",SAVE,360,325,120,32,WS_TABSTOP | BS_DEFPUSHBUTTON as u32),
                control(hwnd,"BUTTON","改用本机 Claude Code 登录",USE_CODE,20,370,460,32,WS_TABSTOP),
            ];
            for field in fields { SendMessageW(field, WM_SETFONT, font as usize, 1); }
            SendMessageW(GetDlgItem(hwnd,KEY), EM_SETLIMITTEXT, 8220, 0);
            SendMessageW(GetDlgItem(hwnd,ORG), EM_SETLIMITTEXT, 128, 0);
            ShowWindow(GetDlgItem(hwnd,ORG),SW_HIDE); ShowWindow(GetDlgItem(hwnd,ORG_LABEL),SW_HIDE);
            note(hwnd, "sessionKey 是敏感登录凭据，请勿分享。\r\n本机加密保存，仅发送至 claude.ai，不上传给开发者。\r\n保存只检查格式；组织 ID 通常自动获取。");
            if (*state).expired {
                note(hwnd, "网页登录已失效。请在网页重新登录后，复制新的 sessionKey。\r\n原组织 ID 已保留；取消不会改动已保存的信息。");
                SetWindowTextW(GetDlgItem(hwnd,SAVE),wide("更新并使用").as_ptr());
            }
            if let Ok(session) = if (*state).load_saved { load() } else { Err(String::new()) } {
                if !(*state).expired { SetWindowTextW(GetDlgItem(hwnd,KEY), wide(&session.key).as_ptr()); }
                if let Some(org) = session.organization {
                    SetWindowTextW(GetDlgItem(hwnd,ORG),wide(&org).as_ptr());
                    SendMessageW(GetDlgItem(hwnd,ADVANCED), BM_SETCHECK, 1, 0);
                    ShowWindow(GetDlgItem(hwnd,ORG),SW_SHOW); ShowWindow(GetDlgItem(hwnd,ORG_LABEL),SW_SHOW);
                }
            }
            0
        }
        WM_COMMAND => {
            match (wp & 0xffff) as i32 {
                SHOW => { let on=SendMessageW(GetDlgItem(hwnd,SHOW),BM_GETCHECK,0,0)==1;
                    SendMessageW(GetDlgItem(hwnd,KEY),EM_SETPASSWORDCHAR,if on {0} else {0x25cf},0); InvalidateRect(GetDlgItem(hwnd,KEY),null(),1); }
                ADVANCED => { let show=if SendMessageW(GetDlgItem(hwnd,ADVANCED),BM_GETCHECK,0,0)==1 {SW_SHOW} else {SW_HIDE};
                    ShowWindow(GetDlgItem(hwnd,ORG),show); ShowWindow(GetDlgItem(hwnd,ORG_LABEL),show); }
                HELP => { MessageBoxW(hwnd,wide("1. 打开 claude.ai 并登录。\r\n2. 按 F12 打开浏览器开发者工具。\r\n3. 在 Application（应用）→ Cookies → https://claude.ai 中找到 sessionKey。\r\n4. 复制它的 Value（值），粘贴到本窗口。\r\n\r\n支持纯值或 sessionKey=值。不要复制整个 Cookie 列表，也不要分享登录信息。\r\n请选择名称完全一致的 sessionKey，不要选 sessionKeyV3 或 LC 项。\r\n组织 ID 可复制同一 Cookie 列表中的 lastActiveOrg 值。").as_ptr(),wide("如何获取网页登录信息").as_ptr(),MB_OK); }
                BROWSER => { ShellExecuteW(hwnd,wide("open").as_ptr(),wide("https://claude.ai/settings/usage").as_ptr(),null(),null(),SW_SHOWNORMAL); }
                SAVE | 1 => {
                    match Session::from_input(&value(hwnd,KEY),&value(hwnd,ORG)) {
                        Err(_) => note(hwnd,"格式不正确。请粘贴 sessionKey 值；组织 ID 应为字母、数字或连字符。"),
                        Ok(session) => {
                            let result = encode(&session).and_then(|encoded| {
                                std::fs::create_dir_all(path().parent().unwrap()).map_err(|_| "无法创建登录信息目录".to_string())?;
                                crate::json::write_file(&path(),&encoded).map_err(|_| "无法保存登录信息".to_string())
                            });
                            match result {
                                Err(error) => note(hwnd,&error),
                                Ok(()) => { PostMessageW((*state).owner as HWND,WM_APP_WEB_LOGIN,1,0);
                                    MessageBoxW(hwnd,wide(&format!("已保存，将在下次允许刷新时使用网页登录。\r\n{}",(*state).waiting)).as_ptr(),wide("Claude 网页登录").as_ptr(),MB_OK); DestroyWindow(hwnd); }
                            }
                        }
                    }
                }
                USE_CODE => {
                    PostMessageW((*state).owner as HWND,WM_APP_WEB_LOGIN,2,0);
                    DestroyWindow(hwnd);
                }
                CLEAR => {
                    if MessageBoxW(hwnd,wide("清除本机保存的网页登录信息，并切换回 Claude Code 登录？").as_ptr(),wide("清除网页登录信息").as_ptr(),MB_YESNO | MB_ICONQUESTION)==IDYES {
                        match std::fs::remove_file(path()) {
                            Ok(()) => { PostMessageW((*state).owner as HWND,WM_APP_WEB_LOGIN,2,0); DestroyWindow(hwnd); }
                            Err(e) if e.kind()==std::io::ErrorKind::NotFound => { PostMessageW((*state).owner as HWND,WM_APP_WEB_LOGIN,2,0); DestroyWindow(hwnd); }
                            Err(_) => note(hwnd,"无法清除登录信息，请检查文件权限。"),
                        }
                    }
                }
                CANCEL | 2 => { DestroyWindow(hwnd); }
                _ => {}
            }
            0
        }
        WM_DESTROY => { if !state.is_null() { DeleteObject((*state).font); } PostQuitMessage(0); 0 }
        _ => DefWindowProcW(hwnd,msg,wp,lp),
    }
}

pub fn open(owner: HWND, waiting: String, expired: bool) {
    if OPEN.swap(true,Ordering::SeqCst) { return }
    let owner=owner as usize;
    let result=std::thread::Builder::new().name("web-login".into()).spawn(move || unsafe {
        let class=wide("IUsageWebLogin");
        let mut wc: WNDCLASSW=std::mem::zeroed(); wc.lpfnWndProc=Some(procedure); wc.hInstance=GetModuleHandleW(null());
        wc.lpszClassName=class.as_ptr(); wc.hCursor=LoadCursorW(null_mut(),IDC_ARROW); wc.hbrBackground=(COLOR_BTNFACE+1) as HBRUSH;
        RegisterClassW(&wc);
        let mut state=Dialog{owner,waiting,font:null_mut(),load_saved:true,expired};
        let scale=windows_sys::Win32::UI::HiDpi::GetDpiForSystem() as f32/96.0;
        let hwnd=CreateWindowExW(WS_EX_APPWINDOW | WS_EX_CONTROLPARENT,class.as_ptr(),wide("Claude 登录设置 · IUsage").as_ptr(),WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
            CW_USEDEFAULT,CW_USEDEFAULT,(520.0*scale) as i32,(455.0*scale) as i32,null_mut(),null_mut(),wc.hInstance,&mut state as *mut _ as *const c_void);
        if !hwnd.is_null() {
            ShowWindow(hwnd,SW_SHOW); SetForegroundWindow(hwnd);
            let mut msg: MSG=std::mem::zeroed();
            while GetMessageW(&mut msg,null_mut(),0,0)>0 {
                if IsDialogMessageW(hwnd,&msg)==0 { TranslateMessage(&msg); DispatchMessageW(&msg); }
            }
        }
        OPEN.store(false,Ordering::SeqCst);
    });
    if result.is_err() { OPEN.store(false,Ordering::SeqCst); }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dialog_controls_mask_secrets_and_expand_advanced_without_network() {
        unsafe {
            let class=wide("IUsageWebLoginTest");
            let mut wc: WNDCLASSW=std::mem::zeroed();
            wc.lpfnWndProc=Some(procedure); wc.hInstance=GetModuleHandleW(null()); wc.lpszClassName=class.as_ptr();
            wc.hbrBackground=(COLOR_BTNFACE+1) as HBRUSH;
            assert_ne!(RegisterClassW(&wc),0);
            let mut state=Dialog{owner:0,waiting:String::new(),font:null_mut(),load_saved:false,expired:true};
            let scale=windows_sys::Win32::UI::HiDpi::GetDpiForSystem() as f32/96.0;
            let hwnd=CreateWindowExW(0,class.as_ptr(),wide("Claude 网页登录").as_ptr(),WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
                0,0,(520.0*scale) as i32,(455.0*scale) as i32,null_mut(),null_mut(),wc.hInstance,&mut state as *mut _ as *const c_void);
            assert!(!hwnd.is_null());
            assert_ne!(GetWindowLongW(GetDlgItem(hwnd,KEY),GWL_STYLE) as u32 & ES_PASSWORD as u32,0);
            assert_eq!(GetWindowLongW(GetDlgItem(hwnd,ORG),GWL_STYLE) as u32 & WS_VISIBLE,0);
            SendMessageW(GetDlgItem(hwnd,ADVANCED),BM_SETCHECK,1,0);
            SendMessageW(hwnd,WM_COMMAND,ADVANCED as usize,0);
            assert_ne!(GetWindowLongW(GetDlgItem(hwnd,ORG),GWL_STYLE) as u32 & WS_VISIBLE,0);
            SetWindowTextW(GetDlgItem(hwnd,KEY),wide("demo-session-secret").as_ptr());
            assert_eq!(value(hwnd,KEY),"demo-session-secret");
            let mut bounds: RECT=std::mem::zeroed(); GetClientRect(hwnd,&mut bounds);
            let mut button: RECT=std::mem::zeroed(); GetWindowRect(GetDlgItem(hwnd,USE_CODE),&mut button);
            let mut corner=POINT{x:button.right,y:button.bottom}; ScreenToClient(hwnd,&mut corner);
            assert!(corner.y<=bounds.bottom && corner.x<=bounds.right);
            if let Some(dir)=std::env::var_os("IUSAGE_TEST_RENDER_DIR") {
                let mut surface=Surface::new(bounds.right,bounds.bottom).unwrap();
                SendMessageW(hwnd,WM_PRINT,surface.dc as usize,(PRF_CLIENT | PRF_CHILDREN | PRF_ERASEBKGND) as isize);
                GdiFlush();
                let w=surface.w; let h=surface.h;
                let mut bmp=Vec::new(); bmp.extend_from_slice(b"BM"); bmp.extend_from_slice(&(54+w as u32*h as u32*4).to_le_bytes());
                bmp.extend_from_slice(&[0;4]); bmp.extend_from_slice(&54u32.to_le_bytes()); bmp.extend_from_slice(&40u32.to_le_bytes());
                bmp.extend_from_slice(&w.to_le_bytes()); bmp.extend_from_slice(&(-h).to_le_bytes()); bmp.extend_from_slice(&1u16.to_le_bytes()); bmp.extend_from_slice(&32u16.to_le_bytes()); bmp.extend_from_slice(&[0;24]);
                for pixel in surface.pixels() {bmp.extend_from_slice(&pixel.to_le_bytes());}
                std::fs::write(PathBuf::from(dir).join("web-login.bmp"),bmp).unwrap();
            }
            DestroyWindow(hwnd);
        }
    }
    #[test]
    fn encrypted_session_round_trip_and_tampering() {
        let session=Session::from_input(" sessionKey=test-secret; ","org-1").unwrap();
        let encoded=encode(&session).unwrap();
        assert!(!encoded.contains("test-secret"));
        let decoded=decode(&encoded).unwrap(); assert_eq!(decoded.key,"test-secret"); assert_eq!(decoded.organization.as_deref(),Some("org-1"));
        let mut damaged=encoded.into_bytes(); damaged[0]=if damaged[0]==b'0' {b'1'} else {b'0'};
        assert!(decode(std::str::from_utf8(&damaged).unwrap()).is_err());
        assert!(Session::from_input("sessionKey=x; other=y","").is_err());
        assert!(Session::from_input("x","../org").is_err());
    }
}
