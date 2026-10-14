#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
pub(super) use linux::run;
#[cfg(target_os = "windows")]
pub(super) use windows::run;

struct WindowGeometry {
    scale: f32,
    size: [u32; 2],
    minimum: [u32; 2],
}

fn fit_window(available: [u32; 2], native_scale: f32, chrome: [u32; 2]) -> WindowGeometry {
    let client =
        std::array::from_fn::<_, 2, _>(|axis| available[axis].saturating_sub(chrome[axis]).max(1));
    let native_scale = if native_scale.is_finite() && native_scale > 0.0 {
        native_scale
    } else {
        1.0
    };
    let scale = native_scale
        .min(client[0] as f32 / 850.0)
        .min(client[1] as f32 / 620.0)
        .max(native_scale.min(0.75));
    let fit = |logical: [f32; 2]| {
        std::array::from_fn(|axis| ((logical[axis] * scale).ceil() as u32).clamp(1, client[axis]))
    };
    WindowGeometry {
        scale,
        size: fit([1024.0, 780.0]),
        minimum: fit([850.0, 620.0]),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub(super) fn run(_title: &str, _update: impl FnMut(&egui::Context)) -> anyhow::Result<()> {
    anyhow::bail!("The desktop interface is available on Linux and Windows")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_window_fits_laptop_height_and_desktop_keeps_default() {
        let laptop = fit_window([1366, 768], 1.0, [48, 80]);
        assert_eq!(laptop.size, [1024, 688]);
        assert_eq!(laptop.scale, 1.0);
        assert!(laptop.minimum[1] <= laptop.size[1]);
        let desktop = fit_window([1280, 900], 1.0, [48, 80]);
        assert_eq!(desktop.size, [1024, 780]);
    }

    #[test]
    fn high_dpi_and_tiny_display_constraints_fit_available_space() {
        let high_dpi = fit_window([1920, 1040], 1.5, [40, 64]);
        assert_eq!(high_dpi.scale, 1.5);
        assert!(high_dpi.size[1] + 64 <= 1040);
        let small = fit_window([640, 480], 1.0, [48, 80]);
        assert!(small.size[0] + 48 <= 640 && small.size[1] + 80 <= 480);
        assert!(small.minimum[0] <= small.size[0] && small.minimum[1] <= small.size[1]);
        let zero = fit_window([0, 0], f32::NAN, [48, 80]);
        assert_eq!(zero.size, [1, 1]);
    }
}
