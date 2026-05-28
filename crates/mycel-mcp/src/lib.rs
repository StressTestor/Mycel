pub fn tool_surface_name() -> &'static str {
    mycel_core::CORE_CRATE_NAME
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delegates_to_core_surface() {
        assert_eq!(tool_surface_name(), "mycel-core");
    }
}
