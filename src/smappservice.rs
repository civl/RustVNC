use objc2::rc::Retained;
use objc2_foundation::{NSError, NSString};
use objc2_service_management::{SMAppService, SMAppServiceStatus};

/// Must match the plist name inside `Contents/Library/LaunchAgents/`.
pub const PLIST_NAME: &str = "com.rustvnc.agent.plist";

fn service() -> Retained<SMAppService> {
    // SAFETY: called on the main thread during app startup / menu handling.
    // SMAppService resolves the plist relative to the calling app bundle.
    unsafe { SMAppService::agentServiceWithPlistName(&NSString::from_str(PLIST_NAME)) }
}

pub fn status() -> SMAppServiceStatus {
    // SAFETY: status is a simple property read; main thread.
    unsafe { service().status() }
}

pub fn register() -> Result<(), String> {
    let service = service();
    // SAFETY: main thread, valid service object.
    unsafe { service.registerAndReturnError() }.map_err(|err| ns_error_message(&err))
}

pub fn unregister() -> Result<(), String> {
    let service = service();
    // SAFETY: main thread, valid service object.
    unsafe { service.unregisterAndReturnError() }.map_err(|err| ns_error_message(&err))
}

pub fn open_login_items_settings() {
    // SAFETY: opens System Settings; no inputs.
    unsafe { SMAppService::openSystemSettingsLoginItems() }
}

pub fn status_label(status: SMAppServiceStatus) -> &'static str {
    if status == SMAppServiceStatus::Enabled {
        "已启用"
    } else if status == SMAppServiceStatus::RequiresApproval {
        "需在系统设置中批准"
    } else if status == SMAppServiceStatus::NotFound {
        "未找到 plist（bundle 结构不完整）"
    } else {
        "未注册"
    }
}

fn ns_error_message(err: &NSError) -> String {
    // SAFETY: localizedDescription returns a valid NSString for this NSError.
    let description = err.localizedDescription();
    let utf8 = description.UTF8String();
    if utf8.is_null() {
        "unknown error".to_string()
    } else {
        // SAFETY: UTF8String returns a NUL-terminated C string valid for the
        // lifetime of `description`.
        unsafe { std::ffi::CStr::from_ptr(utf8) }
            .to_string_lossy()
            .into_owned()
    }
}
