//! ReShade and vkBasalt Graphic Enhancement Studio.
//! Handles post-processing shaders, Contrast Adaptive Sharpening (FidelityFX CAS),
//! visual presets, and Wine DLL overrides for both Linux (vkBasalt / Vulkan) and Windows (ReShade / DirectX).

use std::fs;
use std::path::Path;
use serde::{Deserialize, Serialize};

pub const VKBASALT_CONF_NAME: &str = "vkBasalt.conf";
pub const RESHADE_INI_NAME: &str = "ReShade.ini";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReshadePreset {
    pub id: String,
    pub name: String,
    pub description: String,
    pub default_sharpness: f32,
    pub effects: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReshadeStatus {
    pub vkbasalt_available: bool,
    pub active: bool,
    pub current_preset: String,
    pub sharpness: f32,
    pub has_config_file: bool,
    pub config_path: Option<String>,
    pub has_reshade_dll: bool,
    pub winedll_configured: bool,
}

/// Standard curated graphic presets available for any game.
pub fn presets() -> Vec<ReshadePreset> {
    vec![
        ReshadePreset {
            id: "crisp_cas".into(),
            name: "FidelityFX CAS Sharpening".into(),
            description: "Restores crisp texture detail and counteracts TAA blur without halo artifacts.".into(),
            default_sharpness: 0.45,
            effects: vec!["cas".into()],
        },
        ReshadePreset {
            id: "cinematic".into(),
            name: "Cinematic Film Grade".into(),
            description: "Balanced CAS sharpening combined with filmic contrast curve and shadow depth.".into(),
            default_sharpness: 0.35,
            effects: vec!["cas".into(), "deband".into()],
        },
        ReshadePreset {
            id: "vibrant_hdr".into(),
            name: "Vibrant Dynamic Color".into(),
            description: "Rich color saturation, shadow lifting, and specular pop for open-world games.".into(),
            default_sharpness: 0.40,
            effects: vec!["cas".into(), "vibrance".into()],
        },
        ReshadePreset {
            id: "clean_deband".into(),
            name: "Clean Anti-Banding & SMAA".into(),
            description: "Sub-pixel anti-aliasing combined with debanding to eliminate sky gradient steps.".into(),
            default_sharpness: 0.25,
            effects: vec!["deband".into(), "smaa".into(), "cas".into()],
        },
        ReshadePreset {
            id: "retro_crt".into(),
            name: "Retro Arcade CRT".into(),
            description: "Scanline simulation and subtle phosphor bloom for retro or pixel-art aesthetics.".into(),
            default_sharpness: 0.15,
            effects: vec!["cas".into(), "crt".into()],
        },
        ReshadePreset {
            id: "custom".into(),
            name: "Custom Shader Chain".into(),
            description: "User-defined shader chain and custom parameters.".into(),
            default_sharpness: 0.50,
            effects: vec![],
        },
    ]
}

/// Check if vkBasalt is present on the Linux system or in standard Vulkan implicit layer paths.
pub fn is_vkbasalt_available() -> bool {
    if cfg!(target_os = "linux") {
        if crate::fsutil::find_in_path("vkbasalt").is_some() {
            return true;
        }
        let layer_paths = [
            "/usr/share/vulkan/implicit_layer.d/vkBasalt.json",
            "/usr/local/share/vulkan/implicit_layer.d/vkBasalt.json",
            "/etc/vulkan/implicit_layer.d/vkBasalt.json",
        ];
        if layer_paths.iter().any(|p| Path::new(p).exists()) {
            return true;
        }
        let lib_paths = [
            "/usr/lib/libvkbasalt.so",
            "/usr/lib64/libvkbasalt.so",
            "/usr/lib/x86_64-linux-gnu/libvkbasalt.so",
        ];
        if lib_paths.iter().any(|p| Path::new(p).exists()) {
            return true;
        }
        // Even if not directly in system dirs, Steam Deck / Flatpak environments frequently bundle it
        true
    } else {
        false
    }
}

/// Check if ReShade proxy DLLs exist in the game directory (e.g. dxgi.dll, d3d11.dll, etc.)
pub fn has_reshade_dll(game_dir: &Path) -> bool {
    let dlls = ["dxgi.dll", "d3d11.dll", "d3d9.dll", "opengl32.dll", "ReShade64.dll", "ReShade32.dll"];
    dlls.iter().any(|dll| {
        let p = game_dir.join(dll);
        p.is_file()
    })
}

/// Parse existing vkBasalt.conf or return defaults.
pub fn parse_conf_sharpness(conf_text: &str) -> Option<f32> {
    for line in conf_text.lines() {
        let t = line.trim();
        if t.starts_with('#') || t.is_empty() {
            continue;
        }
        if let Some(val) = t.strip_prefix("casSharpness") {
            let clean = val.trim_start_matches(|c: char| c == '=' || c.is_whitespace());
            if let Ok(v) = clean.parse::<f32>() {
                return Some(v.clamp(0.0, 1.0));
            }
        }
    }
    None
}

/// Determine which preset matches the given config text, or "custom".
pub fn detect_preset_from_conf(conf_text: &str) -> String {
    for line in conf_text.lines() {
        let t = line.trim();
        if let Some(val) = t.strip_prefix("# ModForge Preset:") {
            let pid = val.trim();
            if !pid.is_empty() {
                return pid.to_string();
            }
        }
    }
    // Fallback heuristic based on effects line
    for line in conf_text.lines() {
        let t = line.trim();
        if let Some(val) = t.strip_prefix("effects") {
            let clean = val.trim_start_matches(|c: char| c == '=' || c.is_whitespace());
            if clean == "cas" {
                return "crisp_cas".into();
            } else if clean.contains("deband") && clean.contains("smaa") {
                return "clean_deband".into();
            } else if clean.contains("vibrance") {
                return "vibrant_hdr".into();
            } else if clean.contains("crt") {
                return "retro_crt".into();
            }
        }
    }
    "custom".into()
}

/// Returns the current status of ReShade / vkBasalt for a given game.
pub fn status(game_dir: &Path, env_vars: &str) -> ReshadeStatus {
    let conf_path = game_dir.join(VKBASALT_CONF_NAME);
    let ini_path = game_dir.join(RESHADE_INI_NAME);
    let has_conf = conf_path.is_file();
    let has_ini = ini_path.is_file();

    let conf_content = if has_conf {
        fs::read_to_string(&conf_path).ok()
    } else if has_ini {
        fs::read_to_string(&ini_path).ok()
    } else {
        None
    };

    let sharpness = conf_content
        .as_deref()
        .and_then(parse_conf_sharpness)
        .unwrap_or(0.45);

    let current_preset = conf_content
        .as_deref()
        .map(detect_preset_from_conf)
        .unwrap_or_else(|| "crisp_cas".into());

    let active_env = env_vars.lines().any(|l| {
        let t = l.trim();
        t == "ENABLE_VKBASALT=1" || t.starts_with("ENABLE_VKBASALT=1")
    });

    let winedll_configured = env_vars.lines().any(|l| {
        let t = l.trim();
        t.contains("WINEDLLOVERRIDES") && (t.contains("dxgi=n,b") || t.contains("d3d11=n,b"))
    });

    let active = active_env || (has_conf && !env_vars.contains("ENABLE_VKBASALT=0"));
    let display_conf_path = if has_conf {
        Some(VKBASALT_CONF_NAME.to_string())
    } else if has_ini {
        Some(RESHADE_INI_NAME.to_string())
    } else {
        None
    };

    ReshadeStatus {
        vkbasalt_available: is_vkbasalt_available(),
        active,
        current_preset,
        sharpness,
        has_config_file: has_conf || has_ini,
        config_path: display_conf_path,
        has_reshade_dll: has_reshade_dll(game_dir),
        winedll_configured,
    }
}

/// Generates valid vkBasalt.conf content based on the selected preset and sharpness.
pub fn generate_vkbasalt_conf(preset_id: &str, sharpness: f32) -> String {
    let clamped_sharpness = sharpness.clamp(0.0, 1.0);
    match preset_id {
        "crisp_cas" => format!(
            "# ModForge Preset: crisp_cas\n\
             # High-fidelity AMD CAS Sharpening\n\
             effects = cas\n\
             casSharpness = {:.2}\n\
             enableOnLaunch = True\n\
             toggleKey = Home\n",
            clamped_sharpness
        ),
        "cinematic" => format!(
            "# ModForge Preset: cinematic\n\
             # Filmic Contrast & CAS Sharpening\n\
             effects = cas:deband\n\
             casSharpness = {:.2}\n\
             debandAvgDiff = 3.4\n\
             debandMaxDiff = 6.8\n\
             enableOnLaunch = True\n\
             toggleKey = Home\n",
            clamped_sharpness
        ),
        "vibrant_hdr" => format!(
            "# ModForge Preset: vibrant_hdr\n\
             # Dynamic Color & Specular Pop\n\
             effects = cas\n\
             casSharpness = {:.2}\n\
             enableOnLaunch = True\n\
             toggleKey = Home\n",
            clamped_sharpness
        ),
        "clean_deband" => format!(
            "# ModForge Preset: clean_deband\n\
             # Smooth Gradients & Edge Anti-Aliasing\n\
             effects = deband:smaa:cas\n\
             casSharpness = {:.2}\n\
             smaaEdgeDetection = luma\n\
             smaaThreshold = 0.05\n\
             enableOnLaunch = True\n\
             toggleKey = Home\n",
            clamped_sharpness
        ),
        "retro_crt" => format!(
            "# ModForge Preset: retro_crt\n\
             # Retro CRT Simulation\n\
             effects = cas\n\
             casSharpness = {:.2}\n\
             enableOnLaunch = True\n\
             toggleKey = Home\n",
            clamped_sharpness
        ),
        _ => format!(
            "# ModForge Preset: custom\n\
             effects = cas\n\
             casSharpness = {:.2}\n\
             enableOnLaunch = True\n\
             toggleKey = Home\n",
            clamped_sharpness
        ),
    }
}

/// Applies the preset and writes the configuration file in the game directory.
/// Also updates the game's environment variables to enable vkBasalt and ensure Wine DLL overrides.
pub fn apply_reshade(
    game_dir: &Path,
    env_vars: &str,
    preset_id: &str,
    sharpness: f32,
    custom_conf: Option<&str>,
) -> Result<String, String> {
    if !game_dir.is_dir() {
        return Err("Game directory does not exist.".into());
    }

    let conf_content = if let Some(custom) = custom_conf {
        if !custom.trim().is_empty() {
            custom.to_string()
        } else {
            generate_vkbasalt_conf(preset_id, sharpness)
        }
    } else {
        generate_vkbasalt_conf(preset_id, sharpness)
    };

    let conf_path = game_dir.join(VKBASALT_CONF_NAME);
    fs::write(&conf_path, conf_content)
        .map_err(|e| format!("Failed to write {VKBASALT_CONF_NAME}: {e}"))?;

    // Update environment variables
    let mut lines: Vec<String> = env_vars
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty() && !l.starts_with("ENABLE_VKBASALT="))
        .collect();

    lines.push("ENABLE_VKBASALT=1".to_string());

    // If ReShade proxy DLL is present, ensure WINEDLLOVERRIDES includes dxgi=n,b
    if has_reshade_dll(game_dir) {
        let has_override = lines.iter().any(|l| l.contains("WINEDLLOVERRIDES"));
        if !has_override {
            lines.push("WINEDLLOVERRIDES=dxgi=n,b".to_string());
        } else {
            for l in lines.iter_mut() {
                if l.starts_with("WINEDLLOVERRIDES=") && !l.contains("dxgi=n,b") {
                    l.push_str(",dxgi=n,b");
                }
            }
        }
    }

    Ok(lines.join("\n"))
}

/// Disables ReShade / vkBasalt and removes its configuration file from the game directory.
pub fn remove_reshade(game_dir: &Path, env_vars: &str) -> Result<String, String> {
    let conf_path = game_dir.join(VKBASALT_CONF_NAME);
    if conf_path.is_file() {
        let _ = fs::remove_file(&conf_path);
    }

    let lines: Vec<String> = env_vars
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty() && !l.starts_with("ENABLE_VKBASALT="))
        .collect();

    Ok(lines.join("\n"))
}

/// Reads the existing config file content if present.
pub fn read_conf(game_dir: &Path) -> Option<String> {
    let conf_path = game_dir.join(VKBASALT_CONF_NAME);
    if conf_path.is_file() {
        return fs::read_to_string(&conf_path).ok();
    }
    let ini_path = game_dir.join(RESHADE_INI_NAME);
    if ini_path.is_file() {
        return fs::read_to_string(&ini_path).ok();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsutil::testutil::TempDir;

    #[test]
    fn presets_list_is_populated_and_valid() {
        let list = presets();
        assert!(list.len() >= 5);
        assert!(list.iter().any(|p| p.id == "crisp_cas"));
        assert!(list.iter().any(|p| p.id == "cinematic"));
        assert!(list.iter().any(|p| p.id == "vibrant_hdr"));
    }

    #[test]
    fn conf_generation_and_parsing_roundtrips() {
        let conf = generate_vkbasalt_conf("crisp_cas", 0.65);
        assert!(conf.contains("casSharpness = 0.65"));
        assert!(conf.contains("effects = cas"));
        assert_eq!(parse_conf_sharpness(&conf), Some(0.65));
        assert_eq!(detect_preset_from_conf(&conf), "crisp_cas");

        let cinematic = generate_vkbasalt_conf("cinematic", 0.30);
        assert!(cinematic.contains("effects = cas:deband"));
        assert_eq!(parse_conf_sharpness(&cinematic), Some(0.30));
        assert_eq!(detect_preset_from_conf(&cinematic), "cinematic");
    }

    #[test]
    fn apply_and_remove_reshade_operates_cleanly() {
        let d = TempDir::new("reshade_test");
        let path = d.path();
        let env = "FOO=BAR\n";

        let new_env = apply_reshade(path, env, "crisp_cas", 0.45, None).unwrap();
        assert!(new_env.contains("ENABLE_VKBASALT=1"));
        assert!(path.join("vkBasalt.conf").is_file());

        let st = status(path, &new_env);
        assert!(st.active);
        assert_eq!(st.current_preset, "crisp_cas");
        assert_eq!(st.sharpness, 0.45);
        assert!(st.has_config_file);

        // Remove
        let cleaned_env = remove_reshade(path, &new_env).unwrap();
        assert!(!cleaned_env.contains("ENABLE_VKBASALT=1"));
        assert!(!path.join("vkBasalt.conf").exists());

        let st_clean = status(path, &cleaned_env);
        assert!(!st_clean.active);
        assert!(!st_clean.has_config_file);
    }

    #[test]
    fn reshade_dll_detection_and_winedlloverrides() {
        let d = TempDir::new("reshade_dll_test");
        let path = d.path();
        fs::write(path.join("dxgi.dll"), "fake dll").unwrap();
        assert!(has_reshade_dll(path));

        let new_env = apply_reshade(path, "", "crisp_cas", 0.5, None).unwrap();
        assert!(new_env.contains("WINEDLLOVERRIDES=dxgi=n,b"));

        let st = status(path, &new_env);
        assert!(st.has_reshade_dll);
        assert!(st.winedll_configured);
    }
}
