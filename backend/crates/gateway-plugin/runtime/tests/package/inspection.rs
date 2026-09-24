use gateway_admin::ports::plugins::PluginPackageInspector;
use gateway_plugin_runtime::{PackageInspector, PackageLimits};
use gateway_plugin_sdk::Permission;

#[test]
fn permission_presentations_come_from_the_current_sdk_without_an_archive() {
    let inspector = PackageInspector::new(PackageLimits::default(), "3.14.0".parse().unwrap());
    let permissions = vec!["models".to_owned(), "future_permission".to_owned()];
    let descriptions = inspector.permission_descriptions(&permissions);
    assert_eq!(descriptions[0].permission, "models");
    assert_eq!(descriptions[0].label, Permission::Models.label());
    assert_eq!(
        descriptions[0].description,
        Permission::Models.description()
    );
    assert_eq!(descriptions[1].label, "future_permission");
    assert!(descriptions[1].description.is_empty());
}
