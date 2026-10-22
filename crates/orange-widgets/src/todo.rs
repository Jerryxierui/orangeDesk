//! 待办清单组件数据层（§3 B-3：本地存储）。
//!
//! 设计：trait 化存储 + JSON 文件实现。待办数据量级极小（几十条），
//! JSON 全量原子写（tmp + rename，与内核配置同款纪律）比嵌入 SQLite
//! 更符合 P1 低占用；数据量增长后再按文档迁移 SQLite，trait 边界不变。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 单条待办
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TodoItem {
    pub id: u64,
    pub text: String,
    pub done: bool,
}

/// 待办存储契约
pub trait TodoStore: Send {
    fn list(&self) -> Vec<TodoItem>;
    /// 新增（空文本返回 None）
    fn add(&mut self, text: &str) -> Option<TodoItem>;
    /// 切换完成状态，返回更新后的条目
    fn toggle(&mut self, id: u64) -> Option<TodoItem>;
    fn remove(&mut self, id: u64) -> bool;
}

/// JSON 文件实现（每次变更全量原子写；条目量小，开销可忽略）
pub struct JsonFileTodoStore {
    path: PathBuf,
    items: Vec<TodoItem>,
    next_id: u64,
}

impl JsonFileTodoStore {
    /// 打开（或创建）存储文件
    pub fn open(path: PathBuf) -> Self {
        let (items, next_id) = Self::load(&path);
        Self {
            path,
            items,
            next_id,
        }
    }

    fn load(path: &Path) -> (Vec<TodoItem>, u64) {
        let Ok(raw) = std::fs::read_to_string(path) else {
            return (Vec::new(), 1);
        };
        // 文件损坏不致命：重置为空存储（待办是可再输入数据，不配 crash）
        let Ok(items) = serde_json::from_str::<Vec<TodoItem>>(&raw) else {
            log::warn!("todo store corrupted at {}, starting empty", path.display());
            return (Vec::new(), 1);
        };
        let next_id = items.iter().map(|i| i.id).max().unwrap_or(0) + 1;
        (items, next_id)
    }

    fn persist(&self) {
        let Ok(body) = serde_json::to_string_pretty(&self.items) else {
            return;
        };
        // 原子写：tmp + rename（断电 / 崩溃不留半截文件）
        let tmp = self.path.with_extension("json.tmp");
        if std::fs::write(&tmp, body).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }
}

impl TodoStore for JsonFileTodoStore {
    fn list(&self) -> Vec<TodoItem> {
        self.items.clone()
    }

    fn add(&mut self, text: &str) -> Option<TodoItem> {
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        let item = TodoItem {
            id: self.next_id,
            text: text.to_string(),
            done: false,
        };
        self.next_id += 1;
        self.items.push(item.clone());
        self.persist();
        Some(item)
    }

    fn toggle(&mut self, id: u64) -> Option<TodoItem> {
        let item = self.items.iter_mut().find(|i| i.id == id)?;
        item.done = !item.done;
        let snapshot = item.clone();
        self.persist();
        Some(snapshot)
    }

    fn remove(&mut self, id: u64) -> bool {
        let before = self.items.len();
        self.items.retain(|i| i.id != id);
        let removed = self.items.len() < before;
        if removed {
            self.persist();
        }
        removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("od-todo-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("todos.json")
    }

    #[test]
    fn add_list_toggle_remove_roundtrip() {
        let path = temp_path("crud");
        let mut store = JsonFileTodoStore::open(path.clone());

        let first = store.add("ship v0.3").unwrap();
        store.add("write tests").unwrap();
        assert!(store.add("   ").is_none(), "blank text rejected");
        assert_eq!(store.list().len(), 2);

        let toggled = store.toggle(first.id).unwrap();
        assert!(toggled.done);
        assert!(store.list()[0].done);

        assert!(store.remove(first.id));
        assert!(!store.remove(first.id), "second remove is a no-op");
        assert_eq!(store.list().len(), 1);

        // 持久化 roundtrip：重开拿到剩余条目与 done 状态
        drop(store);
        let reopened = JsonFileTodoStore::open(path);
        let items = reopened.list();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "write tests");
        assert!(!items[0].done);
    }

    #[test]
    fn ids_continue_across_reopen() {
        let path = temp_path("ids");
        let mut store = JsonFileTodoStore::open(path.clone());
        let a = store.add("a").unwrap();
        drop(store);
        let mut reopened = JsonFileTodoStore::open(path);
        let b = reopened.add("b").unwrap();
        assert!(b.id > a.id, "new ids never collide with persisted ones");
    }

    #[test]
    fn corrupted_file_starts_empty() {
        let path = temp_path("corrupt");
        std::fs::write(&path, "{ not json").unwrap();
        let store = JsonFileTodoStore::open(path);
        assert!(store.list().is_empty());
    }

    #[test]
    fn fresh_file_has_no_items() {
        let store = JsonFileTodoStore::open(temp_path("fresh").join("nonexistent.json"));
        assert!(store.list().is_empty());
    }
}
