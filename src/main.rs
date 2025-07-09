#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use crc16::State;
use defer::defer;
use log::{debug, error, info};
use serde::{Deserialize, Serialize};
use simple_error::bail;
use std::collections::HashMap;
use std::error::Error;
use std::fs;
use std::path::PathBuf;
use windows::core::PCWSTR;
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{GetLastError, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Media::Audio::{
    eConsole, ERole, EndpointFormFactor, Headphones, Headset, IMMDeviceEnumerator,
    MMDeviceEnumerator, PKEY_AudioEndpoint_FormFactor, Speakers,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
    STGM_READ,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Variant::{VT_LPWSTR, VT_UI4};
use windows::Win32::UI::Shell::{
    FOLDERID_RoamingAppData, SHGetKnownFolderPath, Shell_NotifyIconW, KNOWN_FOLDER_FLAG, NIF_GUID,
    NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION,
    NIN_SELECT, NOTIFYICONDATAW, NOTIFYICONDATAW_0, NOTIFYICON_VERSION_4,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DispatchMessageW, GetCursorPos,
    GetMenuItemInfoW, GetMessageW, GetWindowLongPtrW, InsertMenuItemW, LoadIconW, PostMessageW,
    PostQuitMessage, RegisterClassExW, SetForegroundWindow, SetMenuItemInfoW, SetWindowLongPtrW,
    TrackPopupMenuEx, UnregisterClassW, GWLP_USERDATA, HICON, HMENU, MENUITEMINFOW, MFS_CHECKED,
    MFS_DISABLED, MFT_SEPARATOR, MFT_STRING, MIIM_FTYPE, MIIM_ID, MIIM_STATE, MIIM_STRING, MSG,
    TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_RIGHTBUTTON, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP,
    WM_CLOSE, WM_COMMAND, WM_DESTROY, WM_QUIT, WM_RBUTTONUP, WNDCLASSEXW,
};
use windows_core::{BOOL, GUID, PWSTR};

mod policy_config;
use policy_config::IPolicyConfig;

const NOTIFY_ICON_GUID: GUID = GUID::from_u128(0x8fc84650_4bca_4125_b778_10313f9623df);

/// Sets the default audio endpoint for the specified role using raw COM interface calls
fn set_default_endpoint(device_id: &str, role: ERole) -> Result<(), Box<dyn Error>> {
    unsafe {
        debug!("Attempting to set default endpoint for device: {device_id}, role: {role:?}",);

        // Create the PolicyConfig instance as IUnknown first
        debug!("Creating PolicyConfig COM instance...");
        let policy_config: IPolicyConfig =
            CoCreateInstance(&policy_config::CLSID_POLICY_CONFIG, None, CLSCTX_ALL)?;

        // Convert device_id to wide string
        let wide_device_id = device_id
            .encode_utf16()
            .chain(Some(0))
            .collect::<Vec<u16>>();
        let pcwstr_device_id = PCWSTR::from_raw(wide_device_id.as_ptr());

        policy_config.SetDefaultEndpoint(pcwstr_device_id, role)?;
        Ok(())
    }
}

/// Gets the current default audio endpoint for debugging
fn get_current_default_endpoint(role: ERole) -> Result<String, Box<dyn Error>> {
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        let device_enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;

        let endpoint = device_enumerator
            .GetDefaultAudioEndpoint(windows::Win32::Media::Audio::eRender, role)?;

        let device_id = endpoint.GetId()?;
        let device_id_str = device_id.to_string()?;

        Ok(device_id_str)
    }
}

fn string_to_tip(s: &str) -> [u16; 128] {
    let mut ret = [0u16; 128];
    let encoded: Vec<u16> = s.encode_utf16().collect();
    assert!(encoded.len() < ret.len());
    for (i, &c) in encoded.iter().enumerate() {
        ret[i] = c;
    }
    ret[encoded.len()] = 0; // Null-terminate the string
    ret
}

#[derive(Debug, Serialize, Deserialize)]
struct AudioDevice {
    id: String,
    friendly_name: String,
    // Whether this device will be included in the rotation.
    selectable: bool,
    #[serde(skip)]
    form_factor: EndpointFormFactor,
}

#[derive(Debug)]
struct AudioSwitch {
    window: HWND,
    icon: HICON,
    popup_menu: HMENU,
    available_devices: Vec<AudioDevice>,

    headphones_icon: HICON,
    headset_icon: HICON,
    speaker_icon: HICON,
}

impl Drop for AudioSwitch {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyMenu(self.popup_menu);
        }
    }
}

impl AudioSwitch {
    #![allow(non_upper_case_globals)]
    fn icon_for_form_factor(&self, form_factor: EndpointFormFactor) -> HICON {
        match form_factor {
            Headphones => self.headphones_icon,
            Headset => self.headset_icon,
            Speakers => self.speaker_icon,
            _ => self.icon, // Default icon for other form factors
        }
    }

    fn current_icon(&self) -> Result<HICON, Box<dyn Error>> {
        let current_device_id = get_current_default_endpoint(eConsole)?;
        let current_device = self
            .available_devices
            .iter()
            .find(|d| d.id == current_device_id)
            .ok_or_else(|| simple_error::SimpleError::new("Current device not found"))?;
        Ok(self.icon_for_form_factor(current_device.form_factor))
    }

    fn show_popup_menu(&self, x: i32, y: i32) -> Result<(), Box<dyn Error>> {
        debug!("Showing popup menu at ({x}, {y})");
        unsafe {
            // Highlight the current device in the popup menu.
            let current_device_id = get_current_default_endpoint(eConsole)?;
            let current_device = self
                .available_devices
                .iter()
                .find(|d| d.id == current_device_id)
                .ok_or_else(|| simple_error::SimpleError::new("Current device not found"))?;
            let mut current_name = current_device
                .friendly_name
                .encode_utf16()
                .chain(Some(0))
                .collect::<Vec<u16>>();

            let mut mii = MENUITEMINFOW {
                cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
                fMask: MIIM_STATE,
                ..Default::default()
            };
            GetMenuItemInfoW(self.popup_menu, POPUP_CURRENT_DEVICE_ID, false, &mut mii)?;
            mii.fMask = MIIM_STRING;
            mii.dwTypeData = PWSTR(current_name.as_mut_ptr());
            mii.dwItemData = current_device.friendly_name.chars().count();
            SetMenuItemInfoW(self.popup_menu, POPUP_CURRENT_DEVICE_ID, false, &mii)?;

            // Required to ensure the popup menu disappears again when a user clicks elsewhere.
            SetForegroundWindow(self.window).ok()?;
            TrackPopupMenuEx(
                self.popup_menu,
                TPM_LEFTALIGN.0 | TPM_BOTTOMALIGN.0 | TPM_RIGHTBUTTON.0,
                x,
                y,
                self.window,
                None,
            )
            .ok()?;
        }
        Ok(())
    }

    fn menu_selection(&mut self, id: u32) -> Result<(), Box<dyn Error>> {
        debug!("Menu item selected: {id}");
        unsafe {
            match id {
                POPUP_EXIT_ID => {
                    debug!("Exit selected");
                    PostMessageW(
                        Some(self.window),
                        WM_CLOSE,
                        WPARAM::default(),
                        LPARAM::default(),
                    )?;
                }
                device_menu_id => {
                    let device = self
                        .available_devices
                        .iter_mut()
                        .find(|device| device_menu_id == device_id_to_menu_id(&device.id));
                    match device {
                        None => {
                            debug!("Unknown menu item selected: {device_menu_id}");
                            return Ok(());
                        }
                        Some(selected_device) => {
                            debug!("Toggling menu item for id: {device_menu_id}");
                            let mut mii = MENUITEMINFOW {
                                cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
                                fMask: MIIM_STATE,
                                ..Default::default()
                            };
                            selected_device.selectable = !selected_device.selectable;
                            GetMenuItemInfoW(self.popup_menu, device_menu_id, false, &mut mii)?;
                            mii.fMask = MIIM_STATE;
                            mii.fState = if selected_device.selectable {
                                mii.fState | MFS_CHECKED
                            } else {
                                mii.fState & !MFS_CHECKED
                            };
                            SetMenuItemInfoW(self.popup_menu, device_menu_id, false, &mii)?;

                            // Save the updated selectable state
                            if let Err(e) = save_device_selectable_state(&self.available_devices) {
                                error!("Failed to save device selectable state: {e}");
                            }
                        }
                    }
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    fn next_device(&mut self) -> Result<(), Box<dyn Error>> {
        let current_device = get_current_default_endpoint(eConsole)?;
        debug!("Switching to next device from: {current_device}");
        let current_index = self
            .available_devices
            .iter()
            .position(|d| d.id == current_device)
            .unwrap_or(0);
        debug!("Current device index: {current_index}");
        let selectable_devices: Vec<_> = self
            .available_devices
            .iter()
            .enumerate()
            .filter(|(_, d)| d.selectable)
            .collect();
        if selectable_devices.is_empty() {
            debug!("No selectable devices found");
            return Ok(());
        }

        let cand = selectable_devices
            .iter()
            // Either the first selectable device after the current one,
            .find(|(i, _)| *i > current_index)
            // or the first selectable device if none found as a wraparound.
            .or_else(|| selectable_devices.first())
            .ok_or_else(|| simple_error::SimpleError::new("No selectable devices found"))?;
        debug!(
            "Switching to device: {:?} at index: {:?}",
            cand.1.friendly_name, cand.0,
        );
        set_default_endpoint(&cand.1.id, eConsole)?;
        // Update the tooltip to reflect the new current device.
        let tooltip = cand.1.friendly_name.clone();
        unsafe {
            Shell_NotifyIconW(
                NIM_MODIFY,
                &NOTIFYICONDATAW {
                    cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                    hWnd: self.window,
                    hIcon: self.icon_for_form_factor(cand.1.form_factor),
                    guidItem: NOTIFY_ICON_GUID,
                    // Both NIF_TIP & NIF_SHOWTIP are required to actually show the tooltip.
                    uFlags: NIF_ICON | NIF_MESSAGE | NIF_GUID | NIF_TIP | NIF_SHOWTIP,
                    uCallbackMessage: WM_APP + 0x42,
                    szTip: string_to_tip(&tooltip),
                    Anonymous: NOTIFYICONDATAW_0 {
                        uVersion: NOTIFYICON_VERSION_4,
                    },
                    ..Default::default()
                },
            )
            .ok()?;
        }

        Ok(())
    }
}

// Technically, these could collide but it's unlikely.
const POPUP_EXIT_ID: u32 = 1;
const POPUP_CURRENT_DEVICE_ID: u32 = 2;

// Converts a device ID to a unique deterministic 16-bit ID for use in the popup menu.
// This must only use the low 16 bits as it is received via `LOWORD` in the WM_COMMAND callback.
fn device_id_to_menu_id(device_id: &str) -> u32 {
    State::<crc16::ARC>::calculate(device_id.as_bytes()) as u32
}

unsafe fn create_popup_menu(
    devices: &[AudioDevice],
    current_device: &AudioDevice,
) -> Result<HMENU, Box<dyn Error>> {
    unsafe {
        let menu = CreatePopupMenu()?;
        debug!("Popup menu created: {menu:?}");
        // Add a menu item to exit the application.
        let mut exit_name = "Exit".encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
        InsertMenuItemW(
            menu,
            0,
            true,
            &MENUITEMINFOW {
                cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
                fMask: MIIM_FTYPE | MIIM_ID | MIIM_STRING,
                fType: MFT_STRING,
                dwTypeData: PWSTR(exit_name.as_mut_ptr()),
                cch: exit_name.len() as u32 - 1,
                wID: POPUP_EXIT_ID,
                ..Default::default()
            },
        )?;
        // Add a separator.
        InsertMenuItemW(
            menu,
            0,
            true,
            &MENUITEMINFOW {
                cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
                fMask: MIIM_FTYPE,
                fType: MFT_SEPARATOR,
                ..Default::default()
            },
        )?;

        for device in devices.iter().rev() {
            debug!(
                "Adding device to popup menu: {:?} {:?}",
                device.friendly_name,
                device_id_to_menu_id(&device.id)
            );
            let mut device_name = device
                .friendly_name
                .encode_utf16()
                .chain(Some(0))
                .collect::<Vec<u16>>();
            InsertMenuItemW(
                menu,
                0,
                true,
                &MENUITEMINFOW {
                    cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
                    fMask: MIIM_FTYPE | MIIM_ID | MIIM_STRING | MIIM_STATE,
                    fType: MFT_STRING,
                    fState: if device.selectable {
                        windows::Win32::UI::WindowsAndMessaging::MFS_CHECKED
                    } else {
                        windows::Win32::UI::WindowsAndMessaging::MFS_UNCHECKED
                    },
                    dwTypeData: PWSTR(device_name.as_mut_ptr()),
                    cch: device_name.len() as u32 - 1,
                    wID: device_id_to_menu_id(&device.id),
                    ..Default::default()
                },
            )?;
        }
        // Add a separator.
        InsertMenuItemW(
            menu,
            0,
            true,
            &MENUITEMINFOW {
                cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
                fMask: MIIM_FTYPE,
                fType: MFT_SEPARATOR,
                ..Default::default()
            },
        )?;
        // Add an item for the current device.
        let mut current_name = current_device
            .friendly_name
            .encode_utf16()
            .chain(Some(0))
            .collect::<Vec<u16>>();
        InsertMenuItemW(
            menu,
            0,
            true,
            &MENUITEMINFOW {
                cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
                fMask: MIIM_FTYPE | MIIM_STATE | MIIM_STRING | MIIM_ID,
                fType: MFT_STRING,
                dwTypeData: PWSTR(current_name.as_mut_ptr()),
                cch: current_device.friendly_name.chars().count() as u32,
                fState: MFS_DISABLED,
                wID: POPUP_CURRENT_DEVICE_ID,
                ..Default::default()
            },
        )?;
        // Add a nice name to the top of the menu.
        let mut title_name = "AudioSwitch"
            .encode_utf16()
            .chain(Some(0))
            .collect::<Vec<u16>>();
        InsertMenuItemW(
            menu,
            0,
            true,
            &MENUITEMINFOW {
                cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
                fMask: MIIM_FTYPE | MIIM_STATE | MIIM_STRING,
                fType: MFT_STRING,
                dwTypeData: PWSTR(title_name.as_mut_ptr()),
                cch: title_name.len() as u32 - 1,
                fState: MFS_DISABLED,
                ..Default::default()
            },
        )?;

        Ok(menu)
    }
}

unsafe fn propvariant_to_string(propvar: &PROPVARIANT) -> Result<String, Box<dyn Error>> {
    unsafe {
        match propvar.vt() {
            VT_LPWSTR => Ok(String::from_utf16_lossy(
                propvar.Anonymous.Anonymous.Anonymous.pwszVal.as_wide(),
            )),
            _ => {
                bail!("Unsupported PROPVARIANT type: {:?}", propvar.vt());
            }
        }
    }
}

fn get_available_audio_devices() -> Result<Vec<AudioDevice>, Box<dyn Error>> {
    let mut devices = Vec::new();
    unsafe {
        let device_enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let endpoints = device_enumerator.EnumAudioEndpoints(
            windows::Win32::Media::Audio::eRender,
            windows::Win32::Media::Audio::DEVICE_STATE_ACTIVE,
        )?;

        for i in 0..endpoints.GetCount()? {
            let endpoint = endpoints.Item(i)?;
            let device_id = endpoint.GetId()?;
            let device_id_str = device_id.to_string()?;
            let props = endpoint.OpenPropertyStore(STGM_READ)?;
            let friendly_name = props.GetValue(&PKEY_Device_FriendlyName)?;
            let form_factor_var = props.GetValue(&PKEY_AudioEndpoint_FormFactor)?;
            let form_factor: EndpointFormFactor = match form_factor_var.vt() {
                VT_UI4 => {
                    EndpointFormFactor(form_factor_var.Anonymous.Anonymous.Anonymous.ulVal as i32)
                }
                _ => {
                    bail!(
                        "Unsupported PROPVARIANT type for form factor: {:?}",
                        form_factor_var,
                    );
                }
            };
            devices.push(AudioDevice {
                id: device_id_str,
                friendly_name: propvariant_to_string(&friendly_name)?,
                selectable: true,
                form_factor,
            });
        }
    }
    Ok(devices)
}

/// Gets the path to the user's roaming AppData directory
fn get_roaming_appdata_path() -> Result<PathBuf, Box<dyn Error>> {
    unsafe {
        let path_ptr =
            SHGetKnownFolderPath(&FOLDERID_RoamingAppData, KNOWN_FOLDER_FLAG::default(), None)?;

        let path_str = path_ptr.to_string()?;
        let path = PathBuf::from(path_str);

        // Free the memory allocated by SHGetKnownFolderPath
        windows::Win32::System::Com::CoTaskMemFree(Some(path_ptr.as_ptr() as *const _));

        Ok(path)
    }
}

/// Gets the full path to the AudioSwitch configuration file
fn get_config_file_path() -> Result<PathBuf, Box<dyn Error>> {
    let mut path = get_roaming_appdata_path()?;
    path.push("PurpleHatstands");
    path.push("AudioSwitch");

    // Create the directory if it doesn't exist
    if !path.exists() {
        fs::create_dir_all(&path)?;
    }

    path.push("device_config.json");
    debug!("Config file path: {}", path.display());
    Ok(path)
}

/// Saves the selectable state of devices to a JSON file in the roaming AppData directory
fn save_device_selectable_state(devices: &[AudioDevice]) -> Result<(), Box<dyn Error>> {
    let config_path = get_config_file_path()?;

    // Create a map of device_id -> selectable state
    let device_states: HashMap<String, bool> = devices
        .iter()
        .map(|device| (device.id.clone(), device.selectable))
        .collect();

    let json_data = serde_json::to_string_pretty(&device_states)?;
    fs::write(&config_path, json_data)?;

    debug!(
        "Saved device selectable state to: {}",
        config_path.display()
    );
    Ok(())
}

/// Loads the selectable state of devices from the JSON file in the roaming AppData directory
fn load_device_selectable_state() -> Result<HashMap<String, bool>, Box<dyn Error>> {
    let config_path = get_config_file_path()?;

    if !config_path.exists() {
        debug!("Config file does not exist: {}", config_path.display());
        return Ok(HashMap::new());
    }

    let json_data = fs::read_to_string(&config_path)?;
    let device_states: HashMap<String, bool> = serde_json::from_str(&json_data)?;

    debug!(
        "Loaded device selectable state from: {}",
        config_path.display()
    );
    Ok(device_states)
}

/// Applies the loaded selectable state to the current devices
fn apply_device_selectable_state(
    devices: &mut [AudioDevice],
    saved_states: &HashMap<String, bool>,
) {
    for device in devices.iter_mut() {
        if let Some(&selectable) = saved_states.get(&device.id) {
            device.selectable = selectable;
            debug!(
                "Applied selectable state for device {}: {}",
                device.friendly_name, selectable
            );
        }
    }
}

fn is_dark_mode() -> Result<bool, Box<dyn Error>> {
    let theme_key = windows_registry::CURRENT_USER
        .open(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize")?;
    let light_theme = theme_key.get_u32("AppsUseLightTheme")? == 1;
    Ok(!light_theme)
}

unsafe fn load_icon(icon_name: &str) -> Result<HICON, Box<dyn Error>> {
    unsafe {
        let module = GetModuleHandleW(None)?;
        let icon_name_wide: Vec<u16> = icon_name.encode_utf16().chain(Some(0)).collect();
        let icon = LoadIconW(Some(module.into()), PCWSTR(icon_name_wide.as_ptr()))?;
        if icon.is_invalid() {
            bail!("Failed to load icon: {}", icon_name);
        }
        Ok(icon)
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    env_logger::init();
    info!("Audio Switch Tool");
    unsafe {
        debug!("Dark mode: {}", is_dark_mode()?);
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        defer!({
            CoUninitialize();
        });
        let module = GetModuleHandleW(None)?;
        let class_name = "AudioSwitchTool"
            .encode_utf16()
            .chain(Some(0))
            .collect::<Vec<u16>>();
        // Register a window class for the taskbar icon.
        let class = RegisterClassExW(&WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(window_callback),
            hInstance: module.into(),
            lpszClassName: PCWSTR(class_name.as_ptr()),
            ..Default::default()
        });
        debug!("Class registered: {class:?}");
        defer!({
            // Unregister the class when done.
            let _ = UnregisterClassW(PCWSTR(class_name.as_ptr()), Some(module.into()));
        });

        // Seems this needs to _not_ be a message-only window for ShellExecute to work.
        let window_name = "Audio Switch Tool"
            .encode_utf16()
            .chain(Some(0))
            .collect::<Vec<u16>>();
        let window = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class as *const u16),
            PCWSTR(window_name.as_ptr()),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            None,
            None,
            Some(module.into()),
            None,
        )
        .inspect_err(|err| {
            error!("Failed to create window: {:?} {:?}", err, GetLastError());
        })?;
        debug!("Window created: {window:?}");
        let mut devices = get_available_audio_devices()?;
        // Load and apply device selectable state
        let saved_states = load_device_selectable_state()?;
        apply_device_selectable_state(&mut devices, &saved_states);
        let current_device_id = get_current_default_endpoint(eConsole)?;
        let current_device = devices
            .iter()
            .find(|d| d.id == current_device_id)
            .ok_or_else(|| simple_error::SimpleError::new("Current device not found"))?;
        let tooltip = current_device.friendly_name.clone();
        let icon = load_icon("audio_icon")?;
        let me = AudioSwitch {
            window,
            icon,
            popup_menu: create_popup_menu(&devices, current_device)?,
            available_devices: devices,
            headphones_icon: load_icon("headphones_icon")?,
            headset_icon: load_icon("headset_icon")?,
            speaker_icon: load_icon("speaker_icon")?,
        };
        // Store the AudioSwitch instance in the window's user data.
        SetWindowLongPtrW(window, GWLP_USERDATA, &me as *const _ as isize);
        let notify_icon_data = &mut NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: window,
            hIcon: me.current_icon()?,
            guidItem: NOTIFY_ICON_GUID,
            // Both NIF_TIP & NIF_SHOWTIP are required to actually show the tooltip.
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_GUID | NIF_TIP | NIF_SHOWTIP,
            uCallbackMessage: WM_APP + 0x42,
            szTip: string_to_tip(&tooltip),
            Anonymous: NOTIFYICONDATAW_0 {
                uVersion: NOTIFYICON_VERSION_4,
            },
            ..Default::default()
        };
        Shell_NotifyIconW(NIM_ADD, notify_icon_data).ok()?;
        defer!({
            // Remove the icon when done.
            debug!("Removing taskbar icon");
            let _ = Shell_NotifyIconW(
                NIM_DELETE,
                &NOTIFYICONDATAW {
                    cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                    uFlags: NIF_GUID,
                    hWnd: window,
                    guidItem: NOTIFY_ICON_GUID,
                    ..Default::default()
                },
            );
        });
        // Enable better callback API.
        Shell_NotifyIconW(NIM_SETVERSION, notify_icon_data).ok()?;

        // Enter the message loop.
        info!("Running...");
        loop {
            let mut msg = MSG::default();
            debug!("Waiting for message...");
            match GetMessageW(&mut msg, None, 0, 0) {
                BOOL(0) => {
                    assert_eq!(msg.message, WM_QUIT);
                    info!("Quitting...");
                    break;
                }
                BOOL(-1) => {
                    error!("Failed to get message: {:?}", GetLastError());
                }
                BOOL(_) => {
                    DispatchMessageW(&msg);
                }
            }
        }
    };

    Ok(())
}

const TASKBAR_CB_ID: u32 = WM_APP + 0x42;
#[allow(non_snake_case)]
pub fn LOWORD(l: isize) -> isize {
    l & 0xffff
}

#[allow(non_snake_case)]
pub fn HIWORD(l: isize) -> isize {
    (l >> 16) & 0xffff
}

unsafe extern "system" fn window_callback(
    hwnd: windows::Win32::Foundation::HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // debug!(
    //     "Window callback: hwnd={:?}, msg={:#x}, wparam={:#x}, lparam={:#x}",
    //     hwnd, msg, wparam.0, lparam.0
    // );
    unsafe {
        let raw_me = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut AudioSwitch;
        match msg {
            TASKBAR_CB_ID => match LOWORD(lparam.0) as u32 {
                WM_RBUTTONUP => {
                    debug!("Right click received");
                    let mut cursor_pos = POINT::default();
                    GetCursorPos(&mut cursor_pos).unwrap();
                    match raw_me
                        .as_mut()
                        .unwrap()
                        .show_popup_menu(cursor_pos.x, cursor_pos.y)
                    {
                        Ok(()) => debug!("Popup menu shown successfully"),
                        Err(e) => error!("Failed to show popup menu: {e:?}"),
                    }
                    LRESULT(0)
                }
                NIN_SELECT => {
                    debug!("NIN_SELECT");
                    match raw_me.as_mut().unwrap().next_device() {
                        Ok(()) => debug!("Popup menu shown successfully"),
                        Err(e) => error!("Failed to show popup menu: {e:?}"),
                    }
                    LRESULT(0)
                }
                _ => DefWindowProcW(hwnd, msg, wparam, lparam),
            },
            WM_COMMAND => {
                debug!("Menu Command received");
                let chosen = LOWORD(wparam.0 as isize) as u32;
                let _ = raw_me.as_mut().unwrap().menu_selection(chosen);
                LRESULT(0)
            }
            WM_DESTROY => {
                // Save the device selectable state on exit
                let _ = save_device_selectable_state(&raw_me.as_ref().unwrap().available_devices);

                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
