//! 自定义列排序器：GtkColumnView 列绑定提供的是 `set_sorter`（GtkSorter），
//! 因此为“名称/路径/修改时间”三列各建一个按 key 比较的 Sorter 子类。

use crate::file_item::FileItem;
use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use std::cell::Cell;

pub type KeyFn = fn(&FileItem, &FileItem) -> gtk::Ordering;

#[derive(Default)]
pub struct KeySorterInner {
    key: Cell<Option<KeyFn>>,
}

#[glib::object_subclass]
impl ObjectSubclass for KeySorterInner {
    const NAME: &'static str = "LseKeySorter";
    type Type = KeySorter;
    type ParentType = gtk::Sorter;
}

impl ObjectImpl for KeySorterInner {}

impl SorterImpl for KeySorterInner {
    fn compare(&self, item1: &glib::Object, item2: &glib::Object) -> gtk::Ordering {
        match (
            item1.downcast_ref::<FileItem>(),
            item2.downcast_ref::<FileItem>(),
            self.key.get(),
        ) {
            (Some(a), Some(b), Some(f)) => f(a, b),
            _ => gtk::Ordering::Equal,
        }
    }
}

glib::wrapper! {
    pub struct KeySorter(ObjectSubclass<KeySorterInner>) @extends gtk::Sorter;
}

impl KeySorter {
    pub fn new(key: KeyFn) -> Self {
        let s: Self = glib::Object::new();
        s.imp().key.set(Some(key));
        s
    }
}

// 三个比较器（返回 gtk::Ordering；“升序点击”下的语义按用户习惯微调）

pub fn cmp_name(a: &FileItem, b: &FileItem) -> gtk::Ordering {
    let (x, y) = (a.name().to_lowercase(), b.name().to_lowercase());
    x.cmp(&y).into()
}

pub fn cmp_path(a: &FileItem, b: &FileItem) -> gtk::Ordering {
    let (x, y) = (a.path().to_lowercase(), b.path().to_lowercase());
    x.cmp(&y).into()
}

/// 修改时间：首次点击“升序”即显示最新在前（时间戳大的视为更小）。
pub fn cmp_mtime(a: &FileItem, b: &FileItem) -> gtk::Ordering {
    b.mtime().cmp(&a.mtime()).into()
}
