//! 结果行控件（gtk::Box 子类）：每行持有自己的列表位置 `pos`，
//! 供右键菜单与拖拽回调在 bind 之后读取（避免 unsafe qdata）。

use gtk::glib;
use gtk::subclass::prelude::*;
use std::cell::Cell;

#[derive(Default)]
pub struct RowInner {
    /// 该行当前对应的列表位置（bind 时更新；u32::MAX = 无效）
    pub pos: Cell<u32>,
}

#[glib::object_subclass]
impl ObjectSubclass for RowInner {
    const NAME: &'static str = "LseRow";
    type Type = Row;
    type ParentType = gtk::Box;
}

impl ObjectImpl for RowInner {}
impl WidgetImpl for RowInner {}
impl BoxImpl for RowInner {}

glib::wrapper! {
    pub struct Row(ObjectSubclass<RowInner>) @extends gtk::Box, gtk::Widget, @implements gtk::Buildable, gtk::ConstraintTarget;
}

impl Row {
    pub fn new() -> Row {
        let row: Row = glib::Object::new();
        gtk::prelude::BoxExt::set_spacing(&row, 6);
        row
    }

    pub fn pos(&self) -> u32 {
        self.imp().pos.get()
    }

    pub fn set_pos(&self, p: u32) {
        self.imp().pos.set(p);
    }
}

impl Default for Row {
    fn default() -> Self {
        Self::new()
    }
}
