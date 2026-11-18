use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RulesConfig {
    pub rules: Vec<ConfigRule>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigRule {
    #[serde(default)]
    pub path: Vec<ConfigPathSegment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ConfigPathSegment {
    Path(PathSegment),
    Wildcard(ConfigPathWildcard),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum PathSegment {
    Field(String),
    Index(usize),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConfigPathWildcard {
    AllItems,
    AllValues,
}

pub fn find_config_violations(config: &Value, rules_config: &RulesConfig) -> Vec<Vec<PathSegment>> {
    let paths = rules_config
        .rules
        .iter()
        .map(|rule| rule.path.as_slice())
        .collect::<Vec<_>>();
    let mut violations = Vec::new();

    collect_violations(config, &paths, &mut Vec::new(), &mut violations);
    violations
}

fn collect_violations(
    value: &Value,
    paths: &[&[ConfigPathSegment]],
    current_path: &mut Vec<PathSegment>,
    violations: &mut Vec<Vec<PathSegment>>,
) {
    if paths.iter().any(|path| path.is_empty()) {
        return;
    }

    match value {
        Value::Object(object) => {
            let supports_object =
                paths
                    .iter()
                    .filter_map(|path| path.split_first())
                    .any(|(segment, _)| {
                        matches!(
                            segment,
                            ConfigPathSegment::Path(PathSegment::Field(_))
                                | ConfigPathSegment::Wildcard(ConfigPathWildcard::AllValues)
                        )
                    });
            if !supports_object {
                violations.push(current_path.clone());
                return;
            }

            for (key, child) in object {
                let child_paths = paths
                    .iter()
                    .filter_map(|path| {
                        let (segment, remainder) = path.split_first()?;
                        match segment {
                            ConfigPathSegment::Path(PathSegment::Field(field)) if field == key => {
                                Some(remainder)
                            }
                            ConfigPathSegment::Wildcard(ConfigPathWildcard::AllValues) => {
                                Some(remainder)
                            }
                            _ => None,
                        }
                    })
                    .collect::<Vec<_>>();

                current_path.push(PathSegment::Field(key.clone()));
                if child_paths.is_empty() {
                    violations.push(current_path.clone());
                } else {
                    collect_violations(child, &child_paths, current_path, violations);
                }
                current_path.pop();
            }
        }
        Value::Array(array) => {
            let supports_array =
                paths
                    .iter()
                    .filter_map(|path| path.split_first())
                    .any(|(segment, _)| {
                        matches!(
                            segment,
                            ConfigPathSegment::Path(PathSegment::Index(_))
                                | ConfigPathSegment::Wildcard(ConfigPathWildcard::AllItems)
                        )
                    });
            if !supports_array {
                violations.push(current_path.clone());
                return;
            }

            for (index, child) in array.iter().enumerate() {
                let child_paths = paths
                    .iter()
                    .filter_map(|path| {
                        let (segment, remainder) = path.split_first()?;
                        match segment {
                            ConfigPathSegment::Path(PathSegment::Index(selected))
                                if *selected == index =>
                            {
                                Some(remainder)
                            }
                            ConfigPathSegment::Wildcard(ConfigPathWildcard::AllItems) => {
                                Some(remainder)
                            }
                            _ => None,
                        }
                    })
                    .collect::<Vec<_>>();

                current_path.push(PathSegment::Index(index));
                if child_paths.is_empty() {
                    violations.push(current_path.clone());
                } else {
                    collect_violations(child, &child_paths, current_path, violations);
                }
                current_path.pop();
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
            violations.push(current_path.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{
        ConfigPathSegment, ConfigPathWildcard, ConfigRule, PathSegment, RulesConfig,
        find_config_violations,
    };

    fn rule(path: Vec<ConfigPathSegment>) -> ConfigRule {
        ConfigRule { path }
    }

    fn field(key: &str) -> ConfigPathSegment {
        ConfigPathSegment::Path(PathSegment::Field(key.into()))
    }

    fn index(index: usize) -> ConfigPathSegment {
        ConfigPathSegment::Path(PathSegment::Index(index))
    }

    fn all_items() -> ConfigPathSegment {
        ConfigPathSegment::Wildcard(ConfigPathWildcard::AllItems)
    }

    fn all_values() -> ConfigPathSegment {
        ConfigPathSegment::Wildcard(ConfigPathWildcard::AllValues)
    }

    fn violation_field(key: &str) -> PathSegment {
        PathSegment::Field(key.into())
    }

    #[test]
    fn returns_no_paths_for_a_config_without_violations() {
        let rules_config = RulesConfig {
            rules: vec![rule(vec![field("agent"), field("name")])],
        };
        let violations = find_config_violations(&json!({"agent": {"name": "Ada"}}), &rules_config);

        assert_eq!(violations, Vec::<Vec<PathSegment>>::new());
        assert_ne!(violations, vec![Vec::<PathSegment>::new()]);
    }

    #[test]
    fn absent_but_permitted_fields_do_not_create_violations() {
        let rules_config = RulesConfig {
            rules: vec![
                rule(vec![field("agent"), field("name")]),
                rule(vec![field("platforms"), field("discord"), field("token")]),
            ],
        };

        assert_eq!(
            find_config_violations(&json!({"agent": {"name": "Ada"}}), &rules_config),
            Vec::<Vec<PathSegment>>::new()
        );
    }

    #[test]
    fn allows_matching_config_and_reports_unlisted_fields() {
        let rules_config = RulesConfig {
            rules: vec![rule(vec![field("agent"), field("name")])],
        };

        assert_eq!(
            find_config_violations(&json!({"agent": {"name": "Ada"}}), &rules_config),
            Vec::<Vec<PathSegment>>::new()
        );
        assert_eq!(
            find_config_violations(
                &json!({"agent": {"name": "Ada", "secret": true}, "storage": {}}),
                &rules_config
            ),
            vec![
                vec![violation_field("agent"), violation_field("secret")],
                vec![violation_field("storage")]
            ]
        );
    }

    #[test]
    fn reports_unselected_array_items_and_distinguishes_map_keys() {
        let config = json!({
            "items": [
                {"name": "first", "secret": true, "0": "map key"},
                {"name": "second"}
            ]
        });
        let rules_config = RulesConfig {
            rules: vec![rule(vec![field("items"), index(0), field("name")])],
        };

        assert_eq!(
            find_config_violations(&config, &rules_config),
            vec![
                vec![
                    violation_field("items"),
                    PathSegment::Index(0),
                    violation_field("secret")
                ],
                vec![
                    violation_field("items"),
                    PathSegment::Index(0),
                    violation_field("0")
                ],
                vec![violation_field("items"), PathSegment::Index(1)]
            ]
        );
    }

    #[test]
    fn all_items_and_all_values_allow_matching_nested_fields() {
        let rules_config = RulesConfig {
            rules: vec![
                rule(vec![field("items"), all_items(), field("name")]),
                rule(vec![field("servers"), all_values(), field("url")]),
            ],
        };
        let allowed = json!({
            "items": [{"name": "first"}, {"name": "second"}],
            "servers": {
                "alpha": {"url": "https://alpha"},
                "beta": {"url": "https://beta"}
            }
        });
        assert_eq!(
            find_config_violations(&allowed, &rules_config),
            Vec::<Vec<PathSegment>>::new()
        );

        let disallowed = json!({
            "items": [{"name": "first", "secret": true}],
            "servers": {"alpha": {"url": "https://alpha", "token": "hidden"}}
        });
        assert_eq!(
            find_config_violations(&disallowed, &rules_config),
            vec![
                vec![
                    violation_field("items"),
                    PathSegment::Index(0),
                    violation_field("secret")
                ],
                vec![
                    violation_field("servers"),
                    violation_field("alpha"),
                    violation_field("token")
                ]
            ]
        );
    }

    #[test]
    fn empty_rules_report_root_and_empty_path_allows_everything() {
        assert_eq!(
            find_config_violations(&json!({"allowed": true}), &RulesConfig::default()),
            vec![Vec::<PathSegment>::new()]
        );

        let rules_config = RulesConfig {
            rules: vec![rule(Vec::new())],
        };
        assert_eq!(
            find_config_violations(&json!({"allowed": true}), &rules_config),
            Vec::<Vec<PathSegment>>::new()
        );
    }

    #[test]
    fn deserializes_and_checks_a_handwritten_json_filter() {
        let source = r#"{
            "rules": [
                {
                    "path": [
                        {"type": "field", "value": "items"},
                        {"type": "index", "value": 0},
                        {"type": "field", "value": "name"}
                    ]
                },
                {
                    "path": [
                        {"type": "field", "value": "groups"},
                        {"type": "all_items"},
                        {"type": "field", "value": "name"}
                    ]
                },
                {
                    "path": [
                        {"type": "field", "value": "mappings"},
                        {"type": "all_values"},
                        {"type": "field", "value": "name"}
                    ]
                }
            ]
        }"#;
        let result = serde_json::from_str::<RulesConfig>(source);
        assert!(result.is_ok(), "handwritten filter JSON should deserialize");

        if let Ok(rules_config) = result {
            let allowed = json!({
                "items": [{"name": "first"}],
                "groups": [{"name": "group"}],
                "mappings": {"alpha": {"name": "mapped"}}
            });
            assert_eq!(
                find_config_violations(&allowed, &rules_config),
                Vec::<Vec<PathSegment>>::new()
            );

            let disallowed = json!({
                "items": [{"name": "first", "secret": true}],
                "groups": [{"name": "group", "token": "hidden"}],
                "mappings": {"alpha": {"name": "mapped", "token": "hidden"}}
            });
            assert_eq!(
                find_config_violations(&disallowed, &rules_config),
                vec![
                    vec![
                        violation_field("items"),
                        PathSegment::Index(0),
                        violation_field("secret")
                    ],
                    vec![
                        violation_field("groups"),
                        PathSegment::Index(0),
                        violation_field("token")
                    ],
                    vec![
                        violation_field("mappings"),
                        violation_field("alpha"),
                        violation_field("token")
                    ]
                ]
            );
        }
    }

    #[test]
    fn handles_empty_containers_at_matching_path_prefixes() {
        let rules_config = RulesConfig {
            rules: vec![rule(vec![field("items"), all_items(), field("name")])],
        };

        assert_eq!(
            find_config_violations(&json!({"items": []}), &rules_config),
            Vec::<Vec<PathSegment>>::new()
        );
        assert_eq!(
            find_config_violations(&Value::Object(serde_json::Map::new()), &rules_config),
            Vec::<Vec<PathSegment>>::new()
        );
        assert_eq!(
            find_config_violations(&json!([]), &rules_config),
            vec![Vec::<PathSegment>::new()]
        );
    }
}
