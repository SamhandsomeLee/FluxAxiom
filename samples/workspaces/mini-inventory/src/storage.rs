//! 库存文件的读写。

use std::fs;
use std::io;
use std::path::Path;

use crate::inventory::Inventory;

/// 读取库存文件。文件不存在时返回空库存。
///
/// 格式错误的行会被跳过。
pub fn load(path: &Path) -> io::Result<Inventory> {
    let mut inventory = Inventory::new();
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(inventory),
        Err(error) => return Err(error),
    };
    for line in text.lines() {
        let mut fields = line.split('\t');
        let (Some(name), Some(quantity), Some(price)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if let (Ok(quantity), Ok(price)) = (quantity.parse(), price.parse()) {
            inventory.add(name, quantity, price);
        }
    }
    Ok(inventory)
}

/// 整体覆盖写入库存文件。
pub fn save(path: &Path, inventory: &Inventory) -> io::Result<()> {
    let mut text = String::new();
    for item in inventory.items() {
        text.push_str(&format!(
            "{}\t{}\t{}\n",
            item.name, item.quantity, item.price_cents
        ));
    }
    fs::write(path, text)
}
