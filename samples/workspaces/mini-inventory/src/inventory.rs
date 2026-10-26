//! 内存中的库存。

/// 一件商品。`price_cents` 是单价，单位为分。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub name: String,
    pub quantity: u32,
    pub price_cents: u64,
}

/// 按名称索引的库存，保持插入顺序。
#[derive(Debug, Default)]
pub struct Inventory {
    items: Vec<Item>,
}

impl Inventory {
    pub fn new() -> Self {
        Self::default()
    }

    /// 入库。同名商品累加数量，并用新单价覆盖旧单价。
    pub fn add(&mut self, name: &str, quantity: u32, price_cents: u64) {
        match self.items.iter_mut().find(|item| item.name == name) {
            Some(item) => {
                item.quantity += quantity;
                item.price_cents = price_cents;
            }
            None => self.items.push(Item {
                name: name.to_owned(),
                quantity,
                price_cents,
            }),
        }
    }

    /// 出库。商品不存在返回 `false`。
    pub fn remove(&mut self, name: &str, quantity: u32) -> bool {
        let Some(item) = self.items.iter_mut().find(|item| item.name == name) else {
            return false;
        };
        item.quantity -= quantity;
        if item.quantity == 0 {
            self.items.retain(|item| item.name != name);
        }
        true
    }

    pub fn items(&self) -> &[Item] {
        &self.items
    }

    /// 全部库存的总价值，单位为分。
    pub fn total_value_cents(&self) -> u64 {
        self.items
            .iter()
            .map(|item| u64::from(item.quantity) * item.price_cents)
            .sum()
    }
}
