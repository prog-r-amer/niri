use std::cell::RefCell;
use std::cmp::max;
use std::collections::HashMap;
use std::iter::zip;
use std::rc::Rc;

use niri_config::{Action, Bind, Config, Key, Modifiers, Trigger};
use pangocairo::cairo::{self, ImageSurface};
use pangocairo::pango::{AttrColor, AttrInt, AttrList, AttrString, FontDescription, Weight};
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::input::keyboard::xkb::keysym_get_name;
use smithay::output::{Output, WeakOutput};
use smithay::reexports::gbm::Format as Fourcc;
use smithay::utils::{Scale, Transform};

use crate::input::CompositorMod;
use crate::render_helpers::primary_gpu_texture::PrimaryGpuTextureRenderElement;
use crate::render_helpers::renderer::NiriRenderer;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::utils::{output_size, to_physical_precise_round};

const PADDING: i32 = 8;
// const MARGIN: i32 = PADDING * 2;
const FONT: &str = "sans 14px";
const BORDER: i32 = 4;
const LINE_INTERVAL: i32 = 2;
const TITLE: &str = "Important Hotkeys";

pub struct HotkeyOverlay {
    is_open: bool,
    config: Rc<RefCell<Config>>,
    comp_mod: CompositorMod,
    buffers: RefCell<HashMap<WeakOutput, RenderedOverlay>>,
}

pub struct RenderedOverlay {
    buffer: Option<TextureBuffer<GlesTexture>>,
}

impl HotkeyOverlay {
    pub fn new(config: Rc<RefCell<Config>>, comp_mod: CompositorMod) -> Self {
        Self {
            is_open: false,
            config,
            comp_mod,
            buffers: RefCell::new(HashMap::new()),
        }
    }

    pub fn show(&mut self) -> bool {
        if !self.is_open {
            self.is_open = true;
            true
        } else {
            false
        }
    }

    pub fn hide(&mut self) -> bool {
        if self.is_open {
            self.is_open = false;
            true
        } else {
            false
        }
    }

    pub fn is_open(&self) -> bool {
        self.is_open
    }

    pub fn on_hotkey_config_updated(&mut self) {
        self.buffers.borrow_mut().clear();
    }

    pub fn render<R: NiriRenderer>(
        &self,
        renderer: &mut R,
        output: &Output,
    ) -> Option<PrimaryGpuTextureRenderElement> {
        if !self.is_open {
            return None;
        }

        let scale = output.current_scale().fractional_scale();
        let output_size = output_size(output);

        let mut buffers = self.buffers.borrow_mut();
        buffers.retain(|output, _| output.is_alive());

        // FIXME: should probably use the working area rather than view size.
        let weak = output.downgrade();
        if let Some(rendered) = buffers.get(&weak) {
            if let Some(buffer) = &rendered.buffer {
                if buffer.texture_scale() != Scale::from(scale) {
                    buffers.remove(&weak);
                }
            }
        }

        let rendered = buffers.entry(weak).or_insert_with(|| {
            let renderer = renderer.as_gles_renderer();
            render(renderer, &self.config.borrow(), self.comp_mod, scale)
                .unwrap_or_else(|_| RenderedOverlay { buffer: None })
        });
        let buffer = rendered.buffer.as_ref()?;

        let size = buffer.logical_size();
        let location = (output_size.to_f64().to_point() - size.to_point()).downscale(2.);
        let mut location = location.to_physical_precise_round(scale).to_logical(scale);
        location.x = f64::max(0., location.x);
        location.y = f64::max(0., location.y);

        let elem = TextureRenderElement::from_texture_buffer(
            buffer.clone(),
            location,
            0.9,
            None,
            None,
            Kind::Unspecified,
        );

        Some(PrimaryGpuTextureRenderElement(elem))
    }
}

fn format_bind(
    binds: &[Bind],
    comp_mod: CompositorMod,
    action: &Action,
) -> Option<(String, String)> {
    let mut bind_with_non_null = None;
    let mut bind_with_custom_title = None;
    let mut found_null_title = false;

    for bind in binds {
        if bind.action != *action {
            continue;
        }

        match &bind.hotkey_overlay_title {
            Some(Some(_)) => {
                bind_with_custom_title.get_or_insert(bind);
            }
            Some(None) => {
                found_null_title = true;
            }
            None => {
                bind_with_non_null.get_or_insert(bind);
            }
        }
    }

    if bind_with_custom_title.is_none() && found_null_title {
        return None;
    }

    let mut title = None;
    let key = if let Some(bind) = bind_with_custom_title.or(bind_with_non_null) {
        if let Some(Some(custom)) = &bind.hotkey_overlay_title {
            title = Some(custom.clone());
        }

        key_name(comp_mod, &bind.key)
    } else {
        String::from("(not bound)")
    };
    let title = title.unwrap_or_else(|| action_name(action));

    Some((format!(" {key} "), title))
}

fn render(
    renderer: &mut GlesRenderer,
    config: &Config,
    comp_mod: CompositorMod,
    scale: f64,
) -> anyhow::Result<RenderedOverlay> {
    let _span = tracy_client::span!("hotkey_overlay::render");

    // let margin = MARGIN * scale;
    let padding: i32 = to_physical_precise_round(scale, PADDING);
    let line_interval: i32 = to_physical_precise_round(scale, LINE_INTERVAL);

    // FIXME: if it doesn't fit, try splitting in two columns or something.
    // let mut target_size = output_size;
    // target_size.w -= margin * 2;
    // target_size.h -= margin * 2;
    // anyhow::ensure!(target_size.w > 0 && target_size.h > 0);

    let binds = &config.binds.0;

    // Collect actions that we want to show.
    let mut actions = vec![&Action::ShowHotkeyOverlay];

    // Prefer Quit(false) if found, otherwise try Quit(true), and if there's neither, fall back to
    // Quit(false).
    if binds.iter().any(|bind| bind.action == Action::Quit(false)) {
        actions.push(&Action::Quit(false));
    } else if binds.iter().any(|bind| bind.action == Action::Quit(true)) {
        actions.push(&Action::Quit(true));
    } else {
        actions.push(&Action::Quit(false));
    }

    actions.extend(&[
        &Action::CloseWindow,
        &Action::FocusColumnLeft,
        &Action::FocusColumnRight,
        &Action::MoveColumnLeft,
        &Action::MoveColumnRight,
        &Action::FocusWorkspaceDown(niri_config::FocusWorkspaceArgument {
            no_mouse_warp: false,
        }),
        &Action::FocusWorkspaceUp(niri_config::FocusWorkspaceArgument {
            no_mouse_warp: false,
        }),
    ]);

    // Prefer move-column-to-workspace-down, but fall back to move-window-to-workspace-down.
    if binds
        .iter()
        .any(|bind| bind.action == Action::MoveColumnToWorkspaceDown)
    {
        actions.push(&Action::MoveColumnToWorkspaceDown);
    } else if binds
        .iter()
        .any(|bind| bind.action == Action::MoveWindowToWorkspaceDown)
    {
        actions.push(&Action::MoveWindowToWorkspaceDown);
    } else {
        actions.push(&Action::MoveColumnToWorkspaceDown);
    }

    // Same for -up.
    if binds
        .iter()
        .any(|bind| bind.action == Action::MoveColumnToWorkspaceUp)
    {
        actions.push(&Action::MoveColumnToWorkspaceUp);
    } else if binds
        .iter()
        .any(|bind| bind.action == Action::MoveWindowToWorkspaceUp)
    {
        actions.push(&Action::MoveWindowToWorkspaceUp);
    } else {
        actions.push(&Action::MoveColumnToWorkspaceUp);
    }

    actions.extend(&[
        &Action::SwitchPresetColumnWidth,
        &Action::MaximizeColumn,
        &Action::ConsumeOrExpelWindowLeft,
        &Action::ConsumeOrExpelWindowRight,
        &Action::ToggleWindowFloating,
        &Action::SwitchFocusBetweenFloatingAndTiling,
    ]);

    // Screenshot is not as important, can omit if not bound.
    if binds.iter().any(|bind| bind.action == Action::Screenshot) {
        actions.push(&Action::Screenshot);
    }

    // Add actions with a custom hotkey-overlay-title.
    for bind in binds {
        if matches!(bind.hotkey_overlay_title, Some(Some(_))) {
            // Avoid duplicate actions.
            if !actions.contains(&&bind.action) {
                actions.push(&bind.action);
            }
        }
    }

    // Add the spawn actions.
    for bind in binds.iter().filter(|bind| {
        matches!(bind.action, Action::Spawn(_))
            // Only show binds with Mod or Super to filter out stuff like volume up/down.
            && (bind.key.modifiers.contains(Modifiers::COMPOSITOR)
                || bind.key.modifiers.contains(Modifiers::SUPER))
            // Also filter out wheel and touchpad scroll binds.
            && matches!(bind.key.trigger, Trigger::Keysym(_))
    }) {
        let action = &bind.action;

        // We only show one bind for each action, so we need to deduplicate the Spawn actions.
        if !actions.contains(&action) {
            actions.push(action);
        }
    }

    let strings = actions
        .into_iter()
        .filter_map(|action| format_bind(binds, comp_mod, action))
        .collect::<Vec<_>>();

    let mut font = FontDescription::from_string(FONT);
    font.set_absolute_size(to_physical_precise_round(scale, font.size()));

    let surface = ImageSurface::create(cairo::Format::ARgb32, 0, 0)?;
    let cr = cairo::Context::new(&surface)?;
    let layout = pangocairo::functions::create_layout(&cr);
    layout.context().set_round_glyph_positions(false);
    layout.set_font_description(Some(&font));

    let bold = AttrList::new();
    bold.insert(AttrInt::new_weight(Weight::Bold));
    layout.set_attributes(Some(&bold));
    layout.set_text(TITLE);
    let title_size = layout.pixel_size();

    let attrs = AttrList::new();
    attrs.insert(AttrString::new_family("Monospace"));
    attrs.insert(AttrColor::new_background(12000, 12000, 12000));

    layout.set_attributes(Some(&attrs));
    let key_sizes = strings
        .iter()
        .map(|(key, _)| {
            layout.set_text(key);
            layout.pixel_size()
        })
        .collect::<Vec<_>>();

    layout.set_attributes(None);
    let action_sizes = strings
        .iter()
        .map(|(_, action)| {
            layout.set_markup(action);
            layout.pixel_size()
        })
        .collect::<Vec<_>>();

    let key_width = key_sizes.iter().map(|(w, _)| w).max().unwrap();
    let action_width = action_sizes.iter().map(|(w, _)| w).max().unwrap();
    let mut width = key_width + padding + action_width;

    let mut height = zip(&key_sizes, &action_sizes)
        .map(|((_, key_h), (_, act_h))| max(key_h, act_h))
        .sum::<i32>()
        + (key_sizes.len() - 1) as i32 * line_interval
        + title_size.1
        + padding;

    width += padding * 2;
    height += padding * 2;

    let surface = ImageSurface::create(cairo::Format::ARgb32, width, height)?;
    let cr = cairo::Context::new(&surface)?;
    cr.set_source_rgb(0.1, 0.1, 0.1);
    cr.paint()?;

    cr.move_to(padding.into(), padding.into());
    let layout = pangocairo::functions::create_layout(&cr);
    layout.context().set_round_glyph_positions(false);
    layout.set_font_description(Some(&font));

    cr.set_source_rgb(1., 1., 1.);

    cr.move_to(((width - title_size.0) / 2).into(), padding.into());
    layout.set_attributes(Some(&bold));
    layout.set_text(TITLE);
    pangocairo::functions::show_layout(&cr, &layout);

    cr.move_to(padding.into(), (padding + title_size.1 + padding).into());

    for ((key, action), ((_, key_h), (_, act_h))) in zip(&strings, zip(&key_sizes, &action_sizes)) {
        layout.set_attributes(Some(&attrs));
        layout.set_text(key);
        pangocairo::functions::show_layout(&cr, &layout);

        cr.rel_move_to((key_width + padding).into(), 0.);

        let (attrs, text) = match pango::parse_markup(action, '\0') {
            Ok((attrs, text, _accel)) => (Some(attrs), text),
            Err(err) => {
                warn!("error parsing markup for key {key}: {err}");
                (None, action.into())
            }
        };

        layout.set_attributes(attrs.as_ref());
        layout.set_text(&text);
        pangocairo::functions::show_layout(&cr, &layout);

        cr.rel_move_to(
            (-(key_width + padding)).into(),
            (max(key_h, act_h) + line_interval).into(),
        );
    }

    cr.move_to(0., 0.);
    cr.line_to(width.into(), 0.);
    cr.line_to(width.into(), height.into());
    cr.line_to(0., height.into());
    cr.line_to(0., 0.);
    cr.set_source_rgb(0.5, 0.8, 1.0);
    // Keep the border width even to avoid blurry edges.
    cr.set_line_width((f64::from(BORDER) / 2. * scale).round() * 2.);
    cr.stroke()?;
    drop(cr);

    let data = surface.take_data().unwrap();
    let buffer = TextureBuffer::from_memory(
        renderer,
        &data,
        Fourcc::Argb8888,
        (width, height),
        false,
        scale,
        Transform::Normal,
        Vec::new(),
    )?;

    Ok(RenderedOverlay {
        buffer: Some(buffer),
    })
}

fn action_name(action: &Action) -> String {
    match action {
        Action::Quit(_) => String::from("Exit niri"),
        Action::ShowHotkeyOverlay => String::from("Show Important Hotkeys"),
        Action::CloseWindow => String::from("Close Focused Window"),
        Action::FocusColumnLeft => String::from("Focus Column to the Left"),
        Action::FocusColumnRight => String::from("Focus Column to the Right"),
        Action::MoveColumnLeft => String::from("Move Column Left"),
        Action::MoveColumnRight => String::from("Move Column Right"),
        Action::FocusWorkspaceDown { .. } => String::from("Switch Workspace Down"),
        Action::FocusWorkspaceUp { .. } => String::from("Switch Workspace Up"),
        Action::MoveColumnToWorkspaceDown => String::from("Move Column to Workspace Down"),
        Action::MoveColumnToWorkspaceUp => String::from("Move Column to Workspace Up"),
        Action::MoveWindowToWorkspaceDown => String::from("Move Window to Workspace Down"),
        Action::MoveWindowToWorkspaceUp => String::from("Move Window to Workspace Up"),
        Action::SwitchPresetColumnWidth => String::from("Switch Preset Column Widths"),
        Action::MaximizeColumn => String::from("Maximize Column"),
        Action::ConsumeOrExpelWindowLeft => String::from("Consume or Expel Window Left"),
        Action::ConsumeOrExpelWindowRight => String::from("Consume or Expel Window Right"),
        Action::ToggleWindowFloating => String::from("Move Window Between Floating and Tiling"),
        Action::SwitchFocusBetweenFloatingAndTiling => {
            String::from("Switch Focus Between Floating and Tiling")
        }
        Action::Screenshot => String::from("Take a Screenshot"),
        Action::Spawn(args) => format!(
            "Spawn <span face='monospace' bgcolor='#000000'>{}</span>",
            args.first().unwrap_or(&String::new())
        ),
        _ => String::from("FIXME: Unknown"),
    }
}

fn key_name(comp_mod: CompositorMod, key: &Key) -> String {
    let mut name = String::new();

    let has_comp_mod = key.modifiers.contains(Modifiers::COMPOSITOR);

    // Compositor mod goes first.
    if has_comp_mod {
        if comp_mod == CompositorMod::Super {
            name.push_str("Super + ");
        } else if comp_mod == CompositorMod::Alt {
            name.push_str("Alt + ");
        }
    }

    if key.modifiers.contains(Modifiers::SUPER)
        && !(has_comp_mod && comp_mod == CompositorMod::Super)
    {
        name.push_str("Super + ");
    }
    if key.modifiers.contains(Modifiers::CTRL) {
        name.push_str("Ctrl + ");
    }
    if key.modifiers.contains(Modifiers::SHIFT) {
        name.push_str("Shift + ");
    }
    if key.modifiers.contains(Modifiers::ALT) && !(has_comp_mod && comp_mod == CompositorMod::Alt) {
        name.push_str("Alt + ");
    }
    if key.modifiers.contains(Modifiers::ISO_LEVEL3_SHIFT) {
        name.push_str("ISO_Level3_Shift + ");
    }
    if key.modifiers.contains(Modifiers::ISO_LEVEL5_SHIFT) {
        name.push_str("ISO_Level5_Shift + ");
    }

    let pretty = match key.trigger {
        Trigger::Keysym(keysym) => prettify_keysym_name(&keysym_get_name(keysym)),
        Trigger::MouseLeft => String::from("Mouse Left"),
        Trigger::MouseRight => String::from("Mouse Right"),
        Trigger::MouseMiddle => String::from("Mouse Middle"),
        Trigger::MouseBack => String::from("Mouse Back"),
        Trigger::MouseForward => String::from("Mouse Forward"),
        Trigger::WheelScrollDown => String::from("Wheel Scroll Down"),
        Trigger::WheelScrollUp => String::from("Wheel Scroll Up"),
        Trigger::WheelScrollLeft => String::from("Wheel Scroll Left"),
        Trigger::WheelScrollRight => String::from("Wheel Scroll Right"),
        Trigger::TouchpadScrollDown => String::from("Touchpad Scroll Down"),
        Trigger::TouchpadScrollUp => String::from("Touchpad Scroll Up"),
        Trigger::TouchpadScrollLeft => String::from("Touchpad Scroll Left"),
        Trigger::TouchpadScrollRight => String::from("Touchpad Scroll Right"),
    };
    name.push_str(&pretty);

    name
}

fn prettify_keysym_name(name: &str) -> String {
    let name = match name {
        "slash" => "/",
        "comma" => ",",
        "period" => ".",
        "minus" => "-",
        "equal" => "=",
        "grave" => "`",
        "bracketleft" => "[",
        "bracketright" => "]",
        "Next" => "Page Down",
        "Prior" => "Page Up",
        "Print" => "PrtSc",
        "Return" => "Enter",
        "space" => "Space",
        _ => name,
    };

    if name.len() == 1 && name.is_ascii() {
        name.to_ascii_uppercase()
    } else {
        name.into()
    }
}

#[cfg(test)]
mod tests {
    use insta::assert_snapshot;

    use super::*;

    #[track_caller]
    fn check(config: &str, action: Action) -> String {
        let config = Config::parse("test.kdl", config).unwrap();
        if let Some((key, title)) = format_bind(&config.binds.0, CompositorMod::Super, &action) {
            format!("{key}: {title}")
        } else {
            String::from("None")
        }
    }

    #[test]
    fn test_format_bind() {
        // Not bound.
        assert_snapshot!(check("", Action::Screenshot), @" (not bound) : Take a Screenshot");

        // Bound with a default title.
        assert_snapshot!(
            check(
                r#"binds {
                    Mod+P { screenshot; }
                }"#,
                Action::Screenshot,
            ),
            @" Super + P : Take a Screenshot"
        );

        // Custom title.
        assert_snapshot!(
            check(
                r#"binds {
                    Mod+P hotkey-overlay-title="Hello" { screenshot; }
                }"#,
                Action::Screenshot,
            ),
            @" Super + P : Hello"
        );

        // Prefer first bind.
        assert_snapshot!(
            check(
                r#"binds {
                    Mod+P { screenshot; }
                    Print { screenshot; }
                }"#,
                Action::Screenshot,
            ),
            @" Super + P : Take a Screenshot"
        );

        // Prefer bind with custom title.
        assert_snapshot!(
            check(
                r#"binds {
                    Mod+P { screenshot; }
                    Print hotkey-overlay-title="My Cool Bind" { screenshot; }
                }"#,
                Action::Screenshot,
            ),
            @" PrtSc : My Cool Bind"
        );

        // Prefer first bind with custom title.
        assert_snapshot!(
            check(
                r#"binds {
                    Mod+P hotkey-overlay-title="First" { screenshot; }
                    Print hotkey-overlay-title="My Cool Bind" { screenshot; }
                }"#,
                Action::Screenshot,
            ),
            @" Super + P : First"
        );

        // Any bind with null title hides it.
        assert_snapshot!(
            check(
                r#"binds {
                    Mod+P { screenshot; }
                    Print hotkey-overlay-title=null { screenshot; }
                }"#,
                Action::Screenshot,
            ),
            @"None"
        );

        // Custom title takes preference over null.
        assert_snapshot!(
            check(
                r#"binds {
                    Mod+P hotkey-overlay-title="Hello" { screenshot; }
                    Print hotkey-overlay-title=null { screenshot; }
                }"#,
                Action::Screenshot,
            ),
            @" Super + P : Hello"
        );
    }
}
