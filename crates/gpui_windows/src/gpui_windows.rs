#![cfg(target_os = "windows")]

mod clipboard;
mod destination_list;
mod direct_manipulation;
mod direct_write;
mod directx_atlas;
mod directx_devices;
mod directx_renderer;
mod dispatcher;
mod display;
mod events;
mod keyboard;
mod platform;
mod system_notifications;
mod system_settings;
mod util;
mod vsync;
mod window;
mod wrapper;

#[cfg(test)]
mod tests {
    use super::direct_manipulation::ContentTransformState;

    #[test]
    fn content_transform_reset_suppresses_reverse_scroll_and_next_pan_uses_identity() {
        let mut state = ContentTransformState::new();
        let (previous, suppressed) = state.observe(1.0, 0.0, -120.0);
        assert!(!suppressed);
        assert_eq!(state.y_offset() - previous.y_offset(), -120.0);

        // A contact during active inertia must keep the prior baseline, so re-grab is -5px.
        state.reconcile_contact_transform(1.0, 0.0, -120.0);
        let (previous, suppressed) = state.observe(1.0, 0.0, -125.0);
        assert!(!suppressed);
        assert_eq!(state.y_offset() - previous.y_offset(), -5.0);

        // A touchpad/pinch contact during active pinch also preserves scale and translation.
        state = ContentTransformState::new();
        let (_, suppressed) = state.observe(1.25, 0.0, -35.0);
        assert!(!suppressed);
        state.reconcile_contact_transform(1.25, 0.0, -35.0);
        let (previous, suppressed) = state.observe(1.3, 0.0, -37.0);
        assert!(!suppressed);
        assert!((state.scale() / previous.scale() - 1.3 / 1.25).abs() < 0.00001);
        assert_eq!(state.y_offset() - previous.y_offset(), -2.0);
        let (previous, suppressed) = state.observe(1.35, 0.0, -38.0);
        assert!(!suppressed);
        assert!((state.scale() / previous.scale() - 1.35 / 1.3).abs() < 0.00001);

        // The READY reset is suppressed until SetContact verifies the real primary transform.
        state = ContentTransformState::new();
        state.observe(1.0, 0.0, -80.0);
        state.start_reset();

        // Update can drain no event while the primary content is still residual at -80.
        state.reconcile_contact_transform(1.0, 0.0, -80.0);
        assert!(state.is_resetting());

        // The delayed identity notification is bookkeeping, not compensating +80 input.
        let (previous, suppressed) = state.observe(1.0, 0.0, 0.0);
        assert!(suppressed);
        assert_eq!(state.y_offset() - previous.y_offset(), 80.0);
        assert!(state.is_resetting());

        // Only a successful primary-content identity read opens the gate.
        state.reconcile_contact_transform(1.0, 0.0, 0.0);
        assert!(!state.is_resetting());

        let (previous, suppressed) = state.observe(1.0, 0.0, -24.0);
        assert!(!suppressed);
        assert_eq!(state.y_offset() - previous.y_offset(), -24.0);
    }

    #[test]
    fn failed_content_reset_preserves_latest_baseline_and_releases_suppression() {
        let mut state = ContentTransformState::new();
        state.observe(1.0, 0.0, -80.0);
        state.start_reset();

        // A synchronous callback may have advanced the transform before ZoomToRect fails.
        let (previous, suppressed) = state.observe(1.0, 0.0, -65.0);
        assert!(suppressed);
        assert_eq!(state.y_offset() - previous.y_offset(), 15.0);

        state.cancel_reset();
        let (previous, suppressed) = state.observe(1.0, 0.0, -70.0);
        assert!(!suppressed);
        assert_eq!(state.y_offset() - previous.y_offset(), -5.0);
    }
}

pub(crate) use clipboard::*;
pub(crate) use destination_list::*;
pub(crate) use direct_write::*;
pub(crate) use directx_atlas::*;
pub(crate) use directx_devices::*;
pub(crate) use directx_renderer::*;
pub(crate) use dispatcher::*;
pub(crate) use display::*;
pub(crate) use events::*;
pub(crate) use keyboard::*;
pub(crate) use platform::*;
pub(crate) use system_notifications::*;
pub(crate) use system_settings::*;
pub(crate) use util::*;
pub(crate) use vsync::*;
pub(crate) use window::*;
pub(crate) use wrapper::*;

pub use platform::WindowsPlatform;

pub(crate) use windows::Win32::Foundation::HWND;
