//! 结果行 GObject（供 Gtk.ColumnView 的列表项使用）。
//! 不注册 GObject 属性——列排序通过自定义 Sorter（见 sorters.rs）完成，
//! 避免属性宏与列绑定 API 的版本耦合。

use gtk::glib;
use gtk::subclass::prelude::*;
use std::cell::{Cell, RefCell};

#[derive(Default)]
pub struct FileItemInner {
    pub path: RefCell<String>,
    pub name: RefCell<String>,
    pub mtime: Cell<i64>,
    pub mtime_text: RefCell<String>,
    pub is_dir: Cell<bool>,
}

#[glib::object_subclass]
impl ObjectSubclass for FileItemInner {
    const NAME: &'static str = "LseFileItem";
    type Type = FileItem;
    type ParentType = glib::Object;
}

impl ObjectImpl for FileItemInner {}

glib::wrapper! {
    pub struct FileItem(ObjectSubclass<FileItemInner>);
}

impl FileItem {
    pub fn new(path: &str, name: &str, mtime: i64, mtime_text: &str, is_dir: bool) -> Self {
        let obj: Self = glib::Object::new();
        obj.inner().path.replace(path.to_string());
        obj.inner().name.replace(name.to_string());
        obj.inner().mtime.set(mtime);
        obj.inner().mtime_text.replace(mtime_text.to_string());
        obj.inner().is_dir.set(is_dir);
        obj
    }

    fn inner(&self) -> &FileItemInner {
        self.imp()
    }

    pub fn path(&self) -> String {
        self.inner().path.borrow().clone()
    }

    pub fn name(&self) -> String {
        self.inner().name.borrow().clone()
    }

    pub fn mtime(&self) -> i64 {
        self.inner().mtime.get()
    }

    pub fn mtime_text(&self) -> String {
        self.inner().mtime_text.borrow().clone()
    }

    pub fn is_dir(&self) -> bool {
        self.inner().is_dir.get()
    }
}
