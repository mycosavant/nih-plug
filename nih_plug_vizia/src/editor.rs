//! The [`Editor`] trait implementation for Vizia editors.

use baseview::{WindowHandle, WindowScalePolicy};
use crossbeam::atomic::AtomicCell;
use nih_plug::debug::*;
use nih_plug::prelude::{Editor, GuiContext, ParentWindowHandle};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use vizia::context::backend::TextConfig;
use vizia::prelude::*;

use crate::widgets::RawParamEvent;
use crate::{assets, widgets, ViziaState, ViziaTheming};

/// An [`Editor`] implementation that calls an vizia draw loop.
pub(crate) struct ViziaEditor {
    pub(crate) vizia_state: Arc<ViziaState>,
    /// The user's app function.
    pub(crate) app: Arc<dyn Fn(&mut Context, Arc<dyn GuiContext>) + 'static + Send + Sync>,
    /// What level of theming to apply. See [`ViziaEditorTheming`].
    pub(crate) theming: ViziaTheming,

    /// The scaling factor reported by the host, if any. On macOS this will never be set and we
    /// should use the system scaling factor instead.
    pub(crate) scaling_factor: AtomicCell<Option<f32>>,

    /// Whether to emit a parameters changed event during the next idle callback. This is set in the
    /// `parameter_values_changed()` implementation and it can be used by widgets to explicitly
    /// check for new parameter values. This is useful when the parameter value is (indirectly) used
    /// to compute a property in an event handler. Like when positioning an element based on the
    /// display value's width.
    pub(crate) emit_parameters_changed_event: Arc<AtomicBool>,

    /// A cross-thread handle into the running vizia context, captured once when the editor is
    /// spawned. This is used by [`Editor::host_resized()`] to push a host-driven resize onto the GUI
    /// thread as a `user_scale_factor` change. It is `None` until the editor is open.
    pub(crate) resize_proxy: Arc<Mutex<Option<ContextProxy>>>,
}

/// Emitted onto the GUI thread by [`ViziaEditor::host_resized()`] and handled by
/// [`WindowModel`][crate::widgets::WindowModel]. Applying the new scale factor mirrors exactly what
/// the [`ResizeHandle`][crate::widgets::ResizeHandle] does, so the rest of the resize plumbing is
/// reused unchanged.
pub(crate) struct HostResizeEvent {
    /// The new uniform user scale factor computed from the host-provided outer window size.
    pub new_user_scale_factor: f64,
}

impl Editor for ViziaEditor {
    fn spawn(
        &self,
        parent: ParentWindowHandle,
        context: Arc<dyn GuiContext>,
    ) -> Box<dyn std::any::Any + Send> {
        let app = self.app.clone();
        let vizia_state = self.vizia_state.clone();
        let theming = self.theming;
        let resize_proxy = self.resize_proxy.clone();

        let (unscaled_width, unscaled_height) = vizia_state.inner_logical_size();
        let system_scaling_factor = self.scaling_factor.load();
        let user_scale_factor = vizia_state.user_scale_factor();

        let mut application = Application::new(move |cx| {
            // Set some default styles to match the iced integration
            if theming >= ViziaTheming::Custom {
                cx.set_default_font(&[assets::NOTO_SANS]);
                if let Err(err) = cx.add_stylesheet(include_style!("assets/theme.css")) {
                    nih_error!("Failed to load stylesheet: {err:?}")
                }

                // There doesn't seem to be any way to bundle styles with a widget, so we'll always
                // include the style sheet for our custom widgets at context creation
                widgets::register_theme(cx);
            }

            // Any widget can change the parameters by emitting `ParamEvent` events. This model will
            // handle them automatically.
            widgets::ParamModel {
                context: context.clone(),
            }
            .build(cx);

            // And we'll link `WindowEvent::ResizeWindow` and `WindowEvent::SetScale` events to our
            // `ViziaState`. We'll notify the host when any of these change.
            let current_inner_window_size = cx.window_size();
            widgets::WindowModel {
                context: context.clone(),
                vizia_state: vizia_state.clone(),
                last_inner_window_size: AtomicCell::new((
                    current_inner_window_size.width,
                    current_inner_window_size.height,
                )),
            }
            .build(cx);

            // Capture a cross-thread proxy so the wrapper can push host-driven resizes onto the GUI
            // thread. See `ViziaEditor::host_resized()`.
            if let Ok(mut slot) = resize_proxy.lock() {
                *slot = Some(cx.get_proxy());
            }

            app(cx, context.clone())
        })
        .with_scale_policy(
            system_scaling_factor
                .map(|factor| WindowScalePolicy::ScaleFactor(factor as f64))
                .unwrap_or(WindowScalePolicy::SystemScaleFactor),
        )
        .inner_size((unscaled_width, unscaled_height))
        .user_scale_factor(user_scale_factor)
        .with_text_config(TextConfig {
            hint: false,
            subpixel: true,
        })
        .on_idle({
            let emit_parameters_changed_event = self.emit_parameters_changed_event.clone();
            move |cx| {
                if emit_parameters_changed_event
                    .compare_exchange(true, false, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok()
                {
                    cx.emit_custom(
                        Event::new(RawParamEvent::ParametersChanged)
                            .propagate(Propagation::Subtree),
                    );
                }
            }
        });

        // This way the plugin can decide to use none of the built in theming
        if theming == ViziaTheming::None {
            application = application.ignore_default_theme();
        }

        let window = application.open_parented(&parent);

        self.vizia_state.open.store(true, Ordering::Release);
        Box::new(ViziaEditorHandle {
            vizia_state: self.vizia_state.clone(),
            window,
            resize_proxy: self.resize_proxy.clone(),
        })
    }

    fn size(&self) -> (u32, u32) {
        // This includes the user scale factor if set, but not any HiDPI scaling
        self.vizia_state.scaled_logical_size()
    }

    fn set_scale_factor(&self, factor: f32) -> bool {
        // If the editor is currently open then the host must not change the current HiDPI scale as
        // we don't have a way to handle that. Ableton Live does this.
        if self.vizia_state.is_open() {
            return false;
        }

        // We're making things a bit more complicated by having both a system scale factor, which is
        // used for HiDPI and also known to the host, and a user scale factor that the user can use
        // to arbitrarily resize the GUI
        self.scaling_factor.store(Some(factor));
        true
    }

    fn param_value_changed(&self, _id: &str, _normalized_value: f32) {
        // This will cause a future idle callback to send a parameters changed event.
        // NOTE: We could add an event containing the parameter's ID and the normalized value, but
        //       these events aren't really necessary for Vizia.
        self.emit_parameters_changed_event
            .store(true, Ordering::Relaxed);
    }

    fn param_modulation_changed(&self, _id: &str, _modulation_offset: f32) {
        self.emit_parameters_changed_event
            .store(true, Ordering::Relaxed);
    }

    fn param_values_changed(&self) {
        self.emit_parameters_changed_event
            .store(true, Ordering::Relaxed);
    }

    fn host_resized(&self, physical_width: u32, physical_height: u32) -> bool {
        let (logical_width, logical_height) = self.vizia_state.inner_logical_size();
        if logical_width == 0 || logical_height == 0 {
            return false;
        }

        // Divide out any HiDPI scaling the host applies so we compute the *user* scale factor, then
        // fit the content into the new box while preserving its aspect ratio. Clamping to a sensible
        // minimum prevents the GUI from becoming unusably small.
        let dpi_factor = self.scaling_factor.load().unwrap_or(1.0) as f64;
        let new_scale = ((physical_width as f64 / dpi_factor) / logical_width as f64)
            .min((physical_height as f64 / dpi_factor) / logical_height as f64)
            .max(0.5);

        match self.resize_proxy.lock() {
            Ok(mut slot) => match slot.as_mut() {
                Some(proxy) => proxy
                    .emit(HostResizeEvent {
                        new_user_scale_factor: new_scale,
                    })
                    .is_ok(),
                None => false,
            },
            Err(_) => false,
        }
    }
}

/// The window handle used for [`ViziaEditor`].
struct ViziaEditorHandle {
    vizia_state: Arc<ViziaState>,
    window: WindowHandle,
    /// The editor's cross-thread resize proxy, cleared when this handle is dropped since the proxy
    /// is tied to the window's event loop.
    resize_proxy: Arc<Mutex<Option<ContextProxy>>>,
}

/// The window handle enum stored within 'WindowHandle' contains raw pointers. Is there a way around
/// having this requirement?
unsafe impl Send for ViziaEditorHandle {}

impl Drop for ViziaEditorHandle {
    fn drop(&mut self) {
        self.vizia_state.open.store(false, Ordering::Release);

        // The proxy points into the event loop of the window that is about to be closed, so it must
        // not outlive it
        if let Ok(mut resize_proxy) = self.resize_proxy.lock() {
            *resize_proxy = None;
        }

        // XXX: This should automatically happen when the handle gets dropped, but apparently not
        self.window.close();
    }
}
