use std::path::Path;
use std::process::ExitCode;

use mini_inventory::storage;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("mini-inventory: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<(), String> {
    let [file, command, rest @ ..] = args else {
        return Err("usage: mini-inventory <file> <add|remove|list|total> ...".to_owned());
    };
    let path = Path::new(file);
    let mut inventory = storage::load(path).map_err(|error| error.to_string())?;
    match (command.as_str(), rest) {
        ("add", [name, quantity, price]) => {
            let quantity = quantity.parse().map_err(|_| "bad quantity")?;
            let price = price.parse().map_err(|_| "bad price")?;
            inventory.add(name, quantity, price);
            storage::save(path, &inventory).map_err(|error| error.to_string())
        }
        ("remove", [name, quantity]) => {
            let quantity = quantity.parse().map_err(|_| "bad quantity")?;
            if !inventory.remove(name, quantity) {
                return Err(format!("no such item: {name}"));
            }
            storage::save(path, &inventory).map_err(|error| error.to_string())
        }
        ("list", []) => {
            for item in inventory.items() {
                println!("{}\t{}\t{}", item.name, item.quantity, item.price_cents);
            }
            Ok(())
        }
        ("total", []) => {
            println!("{}", inventory.total_value_cents());
            Ok(())
        }
        _ => Err("unknown command or wrong argument count".to_owned()),
    }
}
