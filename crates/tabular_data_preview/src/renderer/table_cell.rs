//! Table Cell Rendering

use gpui::{AnyElement, ClipboardItem, ElementId, MouseButton, StatefulInteractiveElement};
use ui::{Color, Divider, Label, LabelSize, SharedString, Tooltip, div, prelude::*};

use crate::{TableView, TableViewEvent, settings::VerticalAlignment, types::DisplayCellId};

/// Adds a right-click-to-copy handler and a tooltip showing `text` plus `hint`
/// (e.g. "Right click to copy content") to a `Stateful` element.
pub(crate) fn with_copy_on_right_click<E: StatefulInteractiveElement>(
    element: E,
    text: SharedString,
    hint: &'static str,
) -> E {
    element
        .on_mouse_down(MouseButton::Right, {
            let text_to_copy = text.clone();
            move |_event, _window, cx| {
                cx.stop_propagation();
                cx.write_to_clipboard(ClipboardItem::new_string(text_to_copy.to_string()));
            }
        })
        .tooltip(Tooltip::element(move |_window, cx| {
            v_flex()
                .gap_1()
                .child(div().font_buffer(cx).child(text.clone()))
                .child(Divider::horizontal())
                .child(Label::new(hint).size(LabelSize::Small).color(Color::Muted))
                .into_any_element()
        }))
}

impl TableView {
    /// Create selectable table cell with mouse event handlers.
    pub fn create_selectable_cell(
        &self,
        display_cell_id: DisplayCellId,
        cell_content: SharedString,
        vertical_alignment: VerticalAlignment,
        cx: &Context<TableView>,
    ) -> AnyElement {
        let Some(data_row) = self.engine.d2d_mapping().get_data_row(display_cell_id.row) else {
            return div().into_any_element();
        };
        let column = display_cell_id.col.0;
        if let Some(edit) = self
            .cell_edit
            .as_ref()
            .filter(|edit| edit.row == data_row.0 && edit.column == column)
        {
            return div()
                .id("table-cell-editor")
                .size_full()
                .min_w_0()
                .border_1()
                .border_color(cx.theme().colors().border_focused)
                .bg(cx.theme().colors().editor_background)
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.emit_cell_edit(cx);
                }))
                .capture_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                    match event.keystroke.key.as_str() {
                        "enter" => this.commit_cell_edit(window, cx),
                        "escape" => this.cancel_cell_edit(window, cx),
                        _ => return,
                    }
                    window.prevent_default();
                    cx.stop_propagation();
                }))
                .child(edit.editor.clone())
                .into_any_element();
        }
        let generation = self.contents_generation;
        create_table_cell(
            display_cell_id,
            cell_content.clone(),
            vertical_alignment,
            cx,
        )
        .when(!self.context_menu_enabled, |cell| {
            with_copy_on_right_click(cell, cell_content, "Right click to copy content")
        })
        .when(self.context_menu_enabled, |cell| {
            cell.on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                    if this.contents_generation == generation {
                        this.request_context_menu(
                            data_row,
                            Some(column),
                            event.position,
                            window,
                            cx,
                        );
                    }
                    cx.stop_propagation();
                }),
            )
        })
        .when(self.selected_rows.contains(&data_row.0), |cell| {
            cell.bg(cx.theme().colors().element_selected)
        })
        .when(
            self.modified_cells.contains(&(data_row.0, column)),
            |cell| cell.bg(cx.theme().status().modified_background),
        )
        .when(self.deleted_rows.contains(&data_row.0), |cell| {
            cell.bg(cx.theme().status().deleted_background)
                .text_color(cx.theme().status().deleted)
        })
        .when(self.selected_cell == Some((data_row.0, column)), |cell| {
            cell.border_1()
                .border_color(cx.theme().colors().border_focused)
        })
        .when(self.selection_enabled || self.editing_enabled, |cell| {
            cell.on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                    if this.contents_generation != generation || this.is_loading {
                        return;
                    }
                    this.select_row(data_row, Some(column), event.modifiers, window, cx);
                    if event.click_count == 2
                        && this.editing_enabled
                        && !this.deleted_rows.contains(&data_row.0)
                    {
                        cx.emit(TableViewEvent::EditCell {
                            row: data_row.0,
                            column,
                        });
                    }
                    cx.stop_propagation();
                }),
            )
        })
        .into_any_element()
    }
}

/// Create styled table cell div element.
fn create_table_cell(
    display_cell_id: DisplayCellId,
    cell_content: SharedString,
    vertical_alignment: VerticalAlignment,
    cx: &Context<'_, TableView>,
) -> gpui::Stateful<Div> {
    let cell = div()
        .id(ElementId::NamedInteger(
            format!("table-display-cell-{}", *display_cell_id.row).into(),
            *display_cell_id.col as u64,
        ))
        .flex()
        .h_full()
        .px_1()
        .border_color(cx.theme().colors().border_variant)
        .map(|div| match vertical_alignment {
            VerticalAlignment::Top => div.items_start(),
            VerticalAlignment::Center => div.items_center(),
        })
        .font_buffer(cx);
    cell.child(div().child(cell_content))
}
