use std::cell::{Cell, RefCell};
use std::rc::Rc;

use anyhow::Result;
use gpui::*;
use gpui_util::ResultExt;
use windows::Win32::{
    Foundation::*,
    Graphics::{DirectManipulation::*, Gdi::*},
    System::Com::*,
    UI::{Input::Pointer::*, WindowsAndMessaging::*},
};

use crate::*;

/// Default viewport size in pixels. The actual content size doesn't matter
/// because we're using the viewport only for gesture recognition, not for
/// visual output.
const DEFAULT_VIEWPORT_SIZE: i32 = 1000;

pub(crate) struct DirectManipulationHandler {
    manager: IDirectManipulationManager,
    update_manager: IDirectManipulationUpdateManager,
    viewport: IDirectManipulationViewport,
    _handler_cookie: u32,
    window: HWND,
    scale_factor: Rc<Cell<f32>>,
    native_touch: bool,
    touch_position: Rc<Cell<Option<Point<Pixels>>>>,
    content_transform: Rc<Cell<ContentTransformState>>,
    pending_events: Rc<RefCell<Vec<PlatformInput>>>,
}

impl DirectManipulationHandler {
    pub fn new(window: HWND, scale_factor: f32, native_touch: bool) -> Result<Self> {
        unsafe {
            let manager: IDirectManipulationManager =
                CoCreateInstance(&DirectManipulationManager, None, CLSCTX_INPROC_SERVER)?;

            let update_manager: IDirectManipulationUpdateManager = manager.GetUpdateManager()?;

            let viewport: IDirectManipulationViewport = manager.CreateViewport(None, window)?;

            let configuration = DIRECTMANIPULATION_CONFIGURATION_INTERACTION
                | DIRECTMANIPULATION_CONFIGURATION_TRANSLATION_X
                | DIRECTMANIPULATION_CONFIGURATION_TRANSLATION_Y
                | DIRECTMANIPULATION_CONFIGURATION_TRANSLATION_INERTIA
                | DIRECTMANIPULATION_CONFIGURATION_RAILS_X
                | DIRECTMANIPULATION_CONFIGURATION_RAILS_Y
                | DIRECTMANIPULATION_CONFIGURATION_SCALING;
            viewport.ActivateConfiguration(configuration)?;

            viewport.SetViewportOptions(
                DIRECTMANIPULATION_VIEWPORT_OPTIONS_MANUALUPDATE
                    | DIRECTMANIPULATION_VIEWPORT_OPTIONS_DISABLEPIXELSNAPPING,
            )?;

            let mut rect = RECT {
                left: 0,
                top: 0,
                right: DEFAULT_VIEWPORT_SIZE,
                bottom: DEFAULT_VIEWPORT_SIZE,
            };
            viewport.SetViewportRect(&mut rect)?;

            manager.Activate(window)?;
            viewport.Enable()?;

            let scale_factor = Rc::new(Cell::new(scale_factor));
            let touch_position = Rc::new(Cell::new(None));
            let content_transform = Rc::new(Cell::new(ContentTransformState::new()));
            let pending_events = Rc::new(RefCell::new(Vec::new()));

            let event_handler: IDirectManipulationViewportEventHandler =
                DirectManipulationEventHandler::new(
                    window,
                    Rc::clone(&scale_factor),
                    Rc::clone(&touch_position),
                    Rc::clone(&content_transform),
                    Rc::clone(&pending_events),
                )
                .into();

            let handler_cookie = viewport.AddEventHandler(Some(window), &event_handler)?;

            update_manager.Update(None)?;

            Ok(Self {
                manager,
                update_manager,
                viewport,
                _handler_cookie: handler_cookie,
                window,
                scale_factor,
                native_touch,
                touch_position,
                content_transform,
                pending_events,
            })
        }
    }

    pub fn set_scale_factor(&self, scale_factor: f32) {
        self.scale_factor.set(scale_factor);
    }

    pub fn on_pointer_down(&self, wparam: WPARAM) {
        if !self.native_touch {
            return;
        }

        unsafe {
            let pointer_id = wparam.loword() as u32;
            let mut pointer_type = POINTER_INPUT_TYPE::default();
            if GetPointerType(pointer_id, &mut pointer_type).is_err() || pointer_type != PT_TOUCH {
                return;
            }

            self.prepare_contact();
            let mut pointer_info = POINTER_INFO::default();
            if GetPointerInfo(pointer_id, &mut pointer_info).is_ok() {
                let mut position = pointer_info.ptPixelLocation;
                if ScreenToClient(self.window, &mut position).as_bool() {
                    self.touch_position.set(Some(logical_point(
                        position.x as f32,
                        position.y as f32,
                        self.scale_factor.get(),
                    )));
                }
            }
            self.viewport.SetContact(pointer_id).log_err();
        }
    }

    pub fn on_pointer_hit_test(&self, wparam: WPARAM) {
        unsafe {
            let pointer_id = wparam.loword() as u32;
            let mut pointer_type = POINTER_INPUT_TYPE::default();
            if GetPointerType(pointer_id, &mut pointer_type).is_ok() && pointer_type == PT_TOUCHPAD
            {
                self.prepare_contact();
                let mut pointer_info = POINTER_INFO::default();
                if GetPointerInfo(pointer_id, &mut pointer_info).is_ok() {
                    let mut point = pointer_info.ptPixelLocation;
                    if ScreenToClient(self.window, &mut point).as_bool() {
                        self.touch_position.set(Some(logical_point(
                            point.x as f32,
                            point.y as f32,
                            self.scale_factor.get(),
                        )));
                    }
                }
                self.viewport.SetContact(pointer_id).log_err();
            }
        }
    }

    pub fn update(&self) {
        unsafe {
            self.update_manager.Update(None).log_err();
        }
    }

    fn prepare_contact(&self) {
        if !self.content_transform.get().resetting {
            // Keep the last pan/pinch baseline when a new contact interrupts active input.
            return;
        }

        // Drain pending reset notifications while suppression is active. Then confirm the
        // viewport's actual primary content transform; READY or Update alone is not a baseline.
        if unsafe { self.update_manager.Update(None) }
            .log_err()
            .is_none()
        {
            return;
        }
        let content: IDirectManipulationContent =
            match unsafe { self.viewport.GetPrimaryContent() }.log_err() {
                Some(content) => content,
                None => return,
            };
        let mut transform = [0.0f32; 6];
        if unsafe { content.GetContentTransform(&mut transform) }
            .log_err()
            .is_none()
            || transform[0] == 0.0
        {
            return;
        }

        let mut state = self.content_transform.get();
        state.reconcile_contact_transform(
            transform[0],
            transform[4] / self.scale_factor.get(),
            transform[5] / self.scale_factor.get(),
        );
        self.content_transform.set(state);
    }

    /// Stop scroll inertia before a touch contact becomes GPUI-owned.
    ///
    /// Native scrolling normally interrupts inertia through `SetContact`. A selected-item drag
    /// deliberately stays out of Direct Manipulation, so it needs to end the previous scroll
    /// sequence explicitly or the old vertical inertia keeps moving underneath the drag.
    pub fn stop_inertia_for_gpui_contact(&self) {
        if !self.native_touch {
            return;
        }

        unsafe {
            // 最新の慣性位置を先に取り込み、Stop 後の Ended と transform reset も反映する。
            self.update_manager.Update(None).log_err();
            if self
                .viewport
                .GetStatus()
                .is_ok_and(should_stop_for_gpui_contact)
            {
                self.viewport.Stop().log_err();
                self.update_manager.Update(None).log_err();
            }
        }
    }

    pub fn drain_events(&self) -> Vec<PlatformInput> {
        std::mem::take(&mut *self.pending_events.borrow_mut())
    }
}

pub(super) fn should_stop_for_gpui_contact(status: DIRECTMANIPULATION_STATUS) -> bool {
    status == DIRECTMANIPULATION_INERTIA
}

impl Drop for DirectManipulationHandler {
    fn drop(&mut self) {
        unsafe {
            self.viewport.Stop().log_err();
            self.viewport.Abandon().log_err();
            self.manager.Deactivate(self.window).log_err();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GestureKind {
    None,
    Scroll,
    Pinch,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct ContentTransformState {
    scale: f32,
    x_offset: f32,
    y_offset: f32,
    resetting: bool,
}

impl ContentTransformState {
    pub(super) fn new() -> Self {
        Self {
            scale: 1.0,
            x_offset: 0.0,
            y_offset: 0.0,
            resetting: false,
        }
    }

    pub(super) fn observe(&mut self, scale: f32, x_offset: f32, y_offset: f32) -> (Self, bool) {
        let previous = *self;
        self.scale = scale;
        self.x_offset = x_offset;
        self.y_offset = y_offset;

        let suppress = self.resetting;
        (previous, suppress)
    }

    pub(super) fn start_reset(&mut self) {
        self.resetting = true;
    }

    pub(super) fn cancel_reset(&mut self) {
        self.resetting = false;
    }

    pub(super) fn reconcile_contact_transform(&mut self, scale: f32, x_offset: f32, y_offset: f32) {
        if !self.resetting {
            return;
        }

        self.scale = scale;
        self.x_offset = x_offset;
        self.y_offset = y_offset;
        if is_identity_transform(scale, x_offset, y_offset) {
            self.resetting = false;
        }
    }

    #[cfg(test)]
    pub(super) fn y_offset(&self) -> f32 {
        self.y_offset
    }

    #[cfg(test)]
    pub(super) fn scale(&self) -> f32 {
        self.scale
    }

    #[cfg(test)]
    pub(super) fn is_resetting(&self) -> bool {
        self.resetting
    }
}

fn is_identity_transform(scale: f32, x_offset: f32, y_offset: f32) -> bool {
    float_equals(scale, 1.0) && float_equals(x_offset, 0.0) && float_equals(y_offset, 0.0)
}

#[windows_core::implement(IDirectManipulationViewportEventHandler)]
struct DirectManipulationEventHandler {
    window: HWND,
    scale_factor: Rc<Cell<f32>>,
    touch_position: Rc<Cell<Option<Point<Pixels>>>>,
    content_transform: Rc<Cell<ContentTransformState>>,
    gesture_kind: Cell<GestureKind>,
    scroll_phase: Cell<TouchPhase>,
    pending_events: Rc<RefCell<Vec<PlatformInput>>>,
}

impl DirectManipulationEventHandler {
    fn new(
        window: HWND,
        scale_factor: Rc<Cell<f32>>,
        touch_position: Rc<Cell<Option<Point<Pixels>>>>,
        content_transform: Rc<Cell<ContentTransformState>>,
        pending_events: Rc<RefCell<Vec<PlatformInput>>>,
    ) -> Self {
        Self {
            window,
            scale_factor,
            touch_position,
            content_transform,
            gesture_kind: Cell::new(GestureKind::None),
            scroll_phase: Cell::new(TouchPhase::Started),
            pending_events,
        }
    }

    fn end_gesture(&self) {
        let position = self.gesture_position();
        let modifiers = current_modifiers();
        match self.gesture_kind.get() {
            GestureKind::Scroll => {
                self.pending_events
                    .borrow_mut()
                    .push(PlatformInput::ScrollWheel(ScrollWheelEvent {
                        position,
                        delta: ScrollDelta::Pixels(point(px(0.0), px(0.0))),
                        modifiers,
                        touch_phase: TouchPhase::Ended,
                    }));
            }
            GestureKind::Pinch => {
                self.pending_events
                    .borrow_mut()
                    .push(PlatformInput::Pinch(PinchEvent {
                        position,
                        delta: 0.0,
                        modifiers,
                        phase: TouchPhase::Ended,
                    }));
            }
            GestureKind::None => {}
        }
        self.gesture_kind.set(GestureKind::None);
    }

    fn gesture_position(&self) -> Point<Pixels> {
        if let Some(position) = self.touch_position.get() {
            return position;
        }
        let scale_factor = self.scale_factor.get();
        unsafe {
            let mut point: POINT = std::mem::zeroed();
            let _ = GetCursorPos(&mut point);
            let _ = ScreenToClient(self.window, &mut point);
            logical_point(point.x as f32, point.y as f32, scale_factor)
        }
    }
}

impl IDirectManipulationViewportEventHandler_Impl for DirectManipulationEventHandler_Impl {
    fn OnViewportStatusChanged(
        &self,
        viewport: windows_core::Ref<'_, IDirectManipulationViewport>,
        current: DIRECTMANIPULATION_STATUS,
        previous: DIRECTMANIPULATION_STATUS,
    ) -> windows_core::Result<()> {
        if current == previous {
            return Ok(());
        }

        // A new gesture interrupted inertia, so end the old sequence.
        if current == DIRECTMANIPULATION_RUNNING && previous == DIRECTMANIPULATION_INERTIA {
            self.end_gesture();
        }

        if current == DIRECTMANIPULATION_READY {
            // Nested READY can be raised by ZoomToRect itself. Keep reset suppression in force
            // until its identity transform notification is observed.
            if self.content_transform.get().resetting {
                return Ok(());
            }
            self.end_gesture();
            // INERTIA が新しい接触で中断される場合、RUNNING への遷移は新しい pointer
            // position を既に記録している。そこで消すと次の scroll が cursor 位置へ
            // 再 hit-test されるため、viewport が完全に idle になった時だけ消す。
            self.touch_position.set(None);

            // Reset the content transform so the viewport is ready for the next gesture.
            // ZoomToRect triggers a second RUNNING -> READY cycle, so prevent an infinite loop here.
            let mut transform = self.content_transform.get();
            if !is_identity_transform(transform.scale, transform.x_offset, transform.y_offset) {
                transform.start_reset();
                self.content_transform.set(transform);
                let reset_requested = viewport.as_ref().and_then(|viewport| unsafe {
                    viewport
                        .ZoomToRect(
                            0.0,
                            0.0,
                            DEFAULT_VIEWPORT_SIZE as f32,
                            DEFAULT_VIEWPORT_SIZE as f32,
                            false,
                        )
                        .log_err()
                });
                if reset_requested.is_none() {
                    // A failed/unavailable reset cannot leave suppression stuck forever. Read
                    // current state after ZoomToRect because synchronous callbacks may update it.
                    let mut transform = self.content_transform.get();
                    transform.cancel_reset();
                    self.content_transform.set(transform);
                }
            } else {
                self.content_transform.set(ContentTransformState::new());
            }
        }

        Ok(())
    }

    fn OnViewportUpdated(
        &self,
        _viewport: windows_core::Ref<'_, IDirectManipulationViewport>,
    ) -> windows_core::Result<()> {
        Ok(())
    }

    fn OnContentUpdated(
        &self,
        _viewport: windows_core::Ref<'_, IDirectManipulationViewport>,
        content: windows_core::Ref<'_, IDirectManipulationContent>,
    ) -> windows_core::Result<()> {
        let content = content.as_ref().ok_or(E_POINTER)?;

        // Get the 6-element content transform: [scale, 0, 0, scale, tx, ty]
        let mut xform = [0.0f32; 6];
        unsafe {
            content.GetContentTransform(&mut xform)?;
        }

        let scale = xform[0];
        let scale_factor = self.scale_factor.get();
        let x_offset = xform[4] / scale_factor;
        let y_offset = xform[5] / scale_factor;

        if scale == 0.0 {
            return Ok(());
        }

        let mut transform = self.content_transform.get();
        let (previous_transform, suppress) = transform.observe(scale, x_offset, y_offset);
        self.content_transform.set(transform);
        if suppress {
            return Ok(());
        }

        let last_scale = previous_transform.scale;
        let last_x = previous_transform.x_offset;
        let last_y = previous_transform.y_offset;

        if float_equals(scale, last_scale)
            && float_equals(x_offset, last_x)
            && float_equals(y_offset, last_y)
        {
            return Ok(());
        }

        let position = self.gesture_position();
        let modifiers = current_modifiers();

        // Direct Manipulation reports both translation and scale in every content update.
        // Translation values can shift during a pinch due to the zoom center shifting.
        // We classify each gesture as either scroll or pinch and only emit one type of event.
        // We allow Scroll -> Pinch (a pinch can start with a small pan) but not the reverse.
        if !float_equals(scale, 1.0) {
            if self.gesture_kind.get() != GestureKind::Pinch {
                self.end_gesture();
                self.gesture_kind.set(GestureKind::Pinch);
                self.pending_events
                    .borrow_mut()
                    .push(PlatformInput::Pinch(PinchEvent {
                        position,
                        delta: 0.0,
                        modifiers,
                        phase: TouchPhase::Started,
                    }));
            }
        } else if self.gesture_kind.get() == GestureKind::None {
            self.gesture_kind.set(GestureKind::Scroll);
            self.scroll_phase.set(TouchPhase::Started);
        }

        match self.gesture_kind.get() {
            GestureKind::Scroll => {
                let dx = x_offset - last_x;
                let dy = y_offset - last_y;
                let touch_phase = self.scroll_phase.get();
                self.scroll_phase.set(TouchPhase::Moved);
                self.pending_events
                    .borrow_mut()
                    .push(PlatformInput::ScrollWheel(ScrollWheelEvent {
                        position,
                        delta: ScrollDelta::Pixels(point(px(dx), px(dy))),
                        modifiers,
                        touch_phase,
                    }));
            }
            GestureKind::Pinch => {
                let scale_delta = scale / last_scale;
                self.pending_events
                    .borrow_mut()
                    .push(PlatformInput::Pinch(PinchEvent {
                        position,
                        delta: scale_delta - 1.0,
                        modifiers,
                        phase: TouchPhase::Moved,
                    }));
            }
            GestureKind::None => {}
        }

        Ok(())
    }
}

fn float_equals(f1: f32, f2: f32) -> bool {
    const EPSILON_SCALE: f32 = 0.00001;
    (f1 - f2).abs() < EPSILON_SCALE * f1.abs().max(f2.abs()).max(EPSILON_SCALE)
}
