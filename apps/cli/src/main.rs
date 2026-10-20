fn startup_line() -> &'static str {
    "FluxAxiom"
}

fn main() {
    println!("{}", startup_line());
}

#[cfg(test)]
mod tests {
    use super::startup_line;

    #[test]
    fn startup_line_is_project_name() {
        assert_eq!(startup_line(), "FluxAxiom");
    }
}
