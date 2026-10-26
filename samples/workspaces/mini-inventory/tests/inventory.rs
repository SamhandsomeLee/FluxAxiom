use mini_inventory::Inventory;

#[test]
fn add_merges_same_name() {
    let mut inventory = Inventory::new();
    inventory.add("pen", 2, 150);
    inventory.add("pen", 3, 200);
    assert_eq!(inventory.items().len(), 1);
    assert_eq!(inventory.items()[0].quantity, 5);
    assert_eq!(inventory.items()[0].price_cents, 200);
}

#[test]
fn total_value_sums_quantity_times_price() {
    let mut inventory = Inventory::new();
    inventory.add("pen", 2, 150);
    inventory.add("book", 1, 1000);
    assert_eq!(inventory.total_value_cents(), 1300);
}

#[test]
fn remove_unknown_item_returns_false() {
    let mut inventory = Inventory::new();
    assert!(!inventory.remove("pen", 1));
}
