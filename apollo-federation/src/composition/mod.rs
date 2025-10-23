mod satisfiability;

use std::collections::HashSet;
use std::vec;

pub use crate::composition::satisfiability::validate_satisfiability;
use crate::error::CompositionError;
pub use crate::schema::schema_upgrader::upgrade_subgraphs_if_necessary;
use crate::subgraph::typestate::Expanded;
use crate::subgraph::typestate::Initial;
use crate::subgraph::typestate::Subgraph;
use crate::subgraph::typestate::Upgraded;
use crate::subgraph::typestate::Validated;
pub use crate::supergraph::Merged;
pub use crate::supergraph::Satisfiable;
pub use crate::supergraph::Supergraph;

/// Options for composition
#[derive(Debug, Clone)]
pub struct CompositionOptions {
    /// Whether to run satisfiability validation (defaults to true)
    pub run_satisfiability: bool,
}

impl Default for CompositionOptions {
    fn default() -> Self {
        Self {
            run_satisfiability: true,
        }
    }
}

/// Main compose function
pub fn compose(
    subgraphs: Vec<Subgraph<Initial>>,
) -> Result<Supergraph<Satisfiable>, Vec<CompositionError>> {
    compose_with_options(subgraphs, CompositionOptions::default())
}

/// Compose with options support
pub fn compose_with_options(
    subgraphs: Vec<Subgraph<Initial>>,
    options: CompositionOptions,
) -> Result<Supergraph<Satisfiable>, Vec<CompositionError>> {
    let expanded_subgraphs = expand_subgraphs(subgraphs)?;
    let upgraded_subgraphs = upgrade_subgraphs_if_necessary(expanded_subgraphs)?;
    let validated_subgraphs = validate_subgraphs(upgraded_subgraphs)?;

    pre_merge_validations(&validated_subgraphs)?;
    let supergraph = merge_subgraphs(validated_subgraphs)?;
    post_merge_validations(&supergraph)?;

    if options.run_satisfiability {
        validate_satisfiability(supergraph)
    } else {
        Ok(supergraph.assume_satisfiable())
    }
}

/// Apollo Federation allow subgraphs to specify partial schemas (i.e. "import" directives through
/// `@link`). This function will update subgraph schemas with all missing federation definitions.
pub fn expand_subgraphs(
    subgraphs: Vec<Subgraph<Initial>>,
) -> Result<Vec<Subgraph<Expanded>>, Vec<CompositionError>> {
    let mut errors: Vec<CompositionError> = vec![];
    let expanded: Vec<Subgraph<Expanded>> = subgraphs
        .into_iter()
        .map(|s| s.expand_links())
        .filter_map(|r| r.map_err(|e| errors.push(e.into())).ok())
        .collect();
    if errors.is_empty() {
        Ok(expanded)
    } else {
        Err(errors)
    }
}

/// Validate subgraph schemas to ensure they satisfy Apollo Federation requirements (e.g. whether
/// `@key` specifies valid `FieldSet`s etc).
pub fn validate_subgraphs(
    subgraphs: Vec<Subgraph<Upgraded>>,
) -> Result<Vec<Subgraph<Validated>>, Vec<CompositionError>> {
    let mut errors: Vec<CompositionError> = vec![];
    let validated: Vec<Subgraph<Validated>> = subgraphs
        .into_iter()
        .map(|s| s.validate())
        .filter_map(|r| r.map_err(|e| errors.push(e.into())).ok())
        .collect();
    if errors.is_empty() {
        Ok(validated)
    } else {
        Err(errors)
    }
}

/// Perform validations that require information about all available subgraphs.
/// 
/// This function runs before merging to catch issues that can only be detected
/// when looking at multiple subgraphs together (e.g., duplicate names, type conflicts).
/// 
/// Note: Individual subgraph validation happens earlier in the pipeline via validate_subgraphs().
/// This function focuses on cross-subgraph consistency checks.
pub fn pre_merge_validations(
    subgraphs: &[Subgraph<Validated>],
) -> Result<(), Vec<CompositionError>> {
    let mut errors = Vec::new();
    
    // Basic sanity check - can't compose nothing
    match subgraphs.len() {
        0 => {
            errors.push(CompositionError::InternalError {
                message: "Cannot compose with empty subgraphs list".to_string(),
            });
        }
        _ => {
            // Use HashSet for O(1) duplicate detection - more efficient than nested loops
            let mut seen_names = HashSet::new();
            for subgraph in subgraphs {
                // HashSet::insert returns false if the value was already present
                match seen_names.insert(&subgraph.name) {
                    false => {
                        errors.push(CompositionError::InternalError {
                            message: format!("Duplicate subgraph name: {}", subgraph.name),
                        });
                    }
                    true => {
                        // Basic URL validation - just check protocol for now
                        // Could use url::Url::parse() for more thorough validation, but keeping it simple
                        match subgraph.url.starts_with("http://") || subgraph.url.starts_with("https://") {
                            false => {
                                errors.push(CompositionError::InternalError {
                                    message: format!(
                                        "Invalid URL format for subgraph '{}': {} (must start with http:// or https://)", 
                                        subgraph.name, 
                                        subgraph.url
                                    ),
                                });
                            }
                            true => {} // Valid URL format
                        }
                    }
                }
            }
        }
    }
    
    // Run additional cross-subgraph validations
    // These catch issues that individual subgraph validation can't detect
    match subgraphs.is_empty() {
        true => {} // Already handled above
        false => {
            // Verify @key directives reference actual fields
            validate_key_fields_across_subgraphs(subgraphs, &mut errors);
            // Catch type kind mismatches (e.g., Object in one subgraph, Enum in another)
            validate_type_conflicts_across_subgraphs(subgraphs, &mut errors);
        }
    }
    
    match errors.is_empty() {
        true => Ok(()),
        false => Err(errors),
    }
}

/// Validate that @key directives reference fields that exist on the type.
///
/// While the subgraph validator catches most @key issues, this provides an additional
/// safety net for edge cases and gives clearer error messages in the composition context.
fn validate_key_fields_across_subgraphs(
    subgraphs: &[Subgraph<Validated>],
    errors: &mut Vec<CompositionError>,
) {
    use apollo_compiler::schema::ExtendedType;
    
    for subgraph in subgraphs {
        let schema = subgraph.schema();
        
        // Check every type in the schema for @key directives
        for (type_name, extended_type) in schema.schema().types.iter() {
            // Check if type has @key directives
            let key_directives: Vec<_> = extended_type
                .directives()
                .iter()
                .filter(|d| d.name == "key")
                .collect();
            
            match key_directives.is_empty() {
                true => {} // Skip types without @key
                false => {
                    // A type can have multiple @key directives - validate each one
                    for key_directive in key_directives {
                        // @key must have a "fields" argument
                        match key_directive.specified_argument_by_name("fields") {
                            None => {
                                errors.push(CompositionError::TypeDefinitionInvalid {
                                    message: format!(
                                        "Subgraph '{}': @key directive on type '{}' is missing 'fields' argument",
                                        subgraph.name,
                                        type_name
                                    ),
                                });
                            }
                            Some(fields_arg) => {
                                // Dereference the Node<Value> to get the actual Value
                                match &**fields_arg {
                                    apollo_compiler::ast::Value::String(field_set) => {
                                        // Validate that the type has fields (for object/interface types)
                                        match extended_type {
                                            ExtendedType::Object(obj_type) => {
                                                validate_key_field_set(
                                                    &subgraph.name,
                                                    type_name,
                                                    field_set,
                                                    &obj_type.fields,
                                                    errors,
                                                );
                                            }
                                            ExtendedType::Interface(interface_type) => {
                                                validate_key_field_set(
                                                    &subgraph.name,
                                                    type_name,
                                                    field_set,
                                                    &interface_type.fields,
                                                    errors,
                                                );
                                            }
                                            _ => {
                                                errors.push(CompositionError::TypeDefinitionInvalid {
                                                    message: format!(
                                                        "Subgraph '{}': @key directive can only be applied to object or interface types, but found on '{}'",
                                                        subgraph.name,
                                                        type_name
                                                    ),
                                                });
                                            }
                                        }
                                    }
                                    _ => {
                                        errors.push(CompositionError::TypeDefinitionInvalid {
                                            message: format!(
                                                "Subgraph '{}': @key directive on type '{}' has invalid 'fields' argument (must be a string)",
                                                subgraph.name,
                                                type_name
                                            ),
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Validate that fields in a @key field set exist on the type.
///
/// This is a simplified parser that handles basic field sets like "id" or "id name".
/// Complex field sets with nested selections (e.g., "id user { name }") would need
/// a proper GraphQL selection set parser, but those are already validated earlier.
fn validate_key_field_set(
    subgraph_name: &str,
    type_name: &apollo_compiler::Name,
    field_set: &str,
    fields: &apollo_compiler::collections::IndexMap<
        apollo_compiler::Name,
        apollo_compiler::schema::Component<apollo_compiler::ast::FieldDefinition>,
    >,
    errors: &mut Vec<CompositionError>,
) {
    // Simple whitespace-based parsing for basic field sets
    let field_names: Vec<&str> = field_set
        .split_whitespace()
        .filter(|s| !s.is_empty())
        .collect();
    
    for field_name in field_names {
        // Clean up any GraphQL syntax characters that might be in the field set
        let clean_field_name = field_name.trim_matches(|c| c == '{' || c == '}' || c == '"');
        
        match clean_field_name.is_empty() {
            true => {} // Skip whitespace-only tokens
            false => {
                // Look up the field in the type's field map
                match fields.get(clean_field_name) {
                    None => {
                        errors.push(CompositionError::TypeDefinitionInvalid {
                            message: format!(
                                "Subgraph '{}': @key directive on type '{}' references non-existent field '{}'",
                                subgraph_name,
                                type_name,
                                clean_field_name
                            ),
                        });
                    }
                    Some(_) => {} // Field exists
                }
            }
        }
    }
}

/// Validate that types with the same name across subgraphs are compatible.
///
/// In Federation, it's valid for multiple subgraphs to define the same type (that's the point!),
/// but they must agree on the type's kind. For example, you can't have "Product" as an Object
/// in one subgraph and an Enum in another.
fn validate_type_conflicts_across_subgraphs(
    subgraphs: &[Subgraph<Validated>],
    errors: &mut Vec<CompositionError>,
) {
    use apollo_compiler::schema::ExtendedType;
    use std::collections::HashMap;
    
    // First pass: collect all type names and their kinds from each subgraph
    let mut type_kinds: HashMap<&apollo_compiler::Name, Vec<(&str, &str)>> = HashMap::new();
    
    for subgraph in subgraphs {
        for (type_name, extended_type) in subgraph.schema().schema().types.iter() {
            let type_kind = match extended_type {
                ExtendedType::Object(_) => "Object",
                ExtendedType::Interface(_) => "Interface",
                ExtendedType::Union(_) => "Union",
                ExtendedType::Enum(_) => "Enum",
                ExtendedType::InputObject(_) => "InputObject",
                ExtendedType::Scalar(_) => "Scalar",
            };
            
            type_kinds
                .entry(type_name)
                .or_insert_with(Vec::new)
                .push((subgraph.name.as_str(), type_kind));
        }
    }
    
    // Second pass: check for conflicts
    for (type_name, subgraph_kinds) in type_kinds.iter() {
        match subgraph_kinds.len() {
            0 | 1 => {} // Single definition, no conflict possible
            _ => {
                // Multiple subgraphs define this type - do they agree on the kind?
                let first_kind = subgraph_kinds[0].1;
                let has_conflict = subgraph_kinds.iter().any(|(_, kind)| *kind != first_kind);
                
                match has_conflict {
                    false => {} // All subgraphs agree
                    true => {
                        let conflict_details: Vec<String> = subgraph_kinds
                            .iter()
                            .map(|(subgraph, kind)| format!("'{}' defines it as {}", subgraph, kind))
                            .collect();
                        
                        errors.push(CompositionError::TypeDefinitionInvalid {
                            message: format!(
                                "Type '{}' has conflicting definitions across subgraphs: {}",
                                type_name,
                                conflict_details.join(", ")
                            ),
                        });
                    }
                }
            }
        }
    }
}

/// Merge validated subgraphs into a single supergraph schema.
///
/// This is where the actual composition happens - combining multiple subgraph schemas
/// into one unified supergraph. The merger handles:
/// - Combining types from different subgraphs
/// - Resolving field conflicts
/// - Adding Federation-specific types (_Entity, _Service, etc.)
pub fn merge_subgraphs(
    subgraphs: Vec<Subgraph<Validated>>,
) -> Result<Supergraph<Merged>, Vec<CompositionError>> {
    use crate::merger::merge::CompositionOptions as MergerOptions;
    use crate::merger::merge::merge_subgraphs as new_merge_subgraphs;

    // Use the new merger implementation (there's an old one too, but this is better)
    let options = MergerOptions::default();
    let merge_result = new_merge_subgraphs(subgraphs, options).map_err(|e| {
        vec![CompositionError::InternalError {
            message: format!("Merge failed: {}", e),
        }]
    })?;

    // Check if the merger reported any errors
    if !merge_result.errors.is_empty() {
        return Err(merge_result.errors);
    }

    // Extract the supergraph schema from the merge result
    if let Some(supergraph_schema) = merge_result.supergraph {
        // Type conversion dance: Valid<FederationSchema> → FederationSchema → Schema → Valid<Schema>
        // This is necessary because the merger returns a different type than what we need
        let schema = supergraph_schema.into_inner().into_inner();
        let valid_schema = apollo_compiler::validation::Valid::assume_valid(schema);

        let supergraph = Supergraph::<Merged>::new(valid_schema);
        Ok(supergraph)
    } else {
        // This shouldn't happen if there are no errors, but handle it just in case
        Err(vec![CompositionError::InternalError {
            message: "Merge completed but no supergraph schema was produced".to_string(),
        }])
    }
}

/// Validate the merged supergraph schema.
///
/// After merging, we need to ensure the resulting supergraph is valid:
/// - Must have a Query type (required by GraphQL spec)
/// - _Entity union must be properly formed (if it exists)
/// - All entity types should have @key directives
/// - No orphaned types that aren't reachable from root types
pub fn post_merge_validations(
    supergraph: &Supergraph<Merged>,
) -> Result<(), Vec<CompositionError>> {
    let schema = supergraph.schema();
    let mut errors = Vec::new();

    // Every GraphQL schema must have a Query type
    match &schema.schema_definition.query {
        None => {
            errors.push(CompositionError::TypeDefinitionInvalid {
                message: "Supergraph must have a query type".to_string(),
            });
        }
        Some(_) => {} // Good to go
    }

    // Validate the _Entity union if it exists (Federation-specific)
    match schema.types.get("_Entity") {
        Some(entity_type) => {
            match entity_type {
                apollo_compiler::schema::ExtendedType::Union(union_type) => {
                    match union_type.members.is_empty() {
                        true => {
                            errors.push(CompositionError::TypeDefinitionInvalid {
                                message: "_Entity union exists but has no members".to_string(),
                            });
                        }
                        false => {} // Union has members
                    }
                }
                _ => {
                    errors.push(CompositionError::TypeDefinitionInvalid {
                        message: "_Entity must be a union type".to_string(),
                    });
                }
            }
        }
        None => {} // No _Entity union, which is fine
    }

    // Additional post-merge validations
    validate_entity_consistency(schema, &mut errors);
    validate_reachable_types(schema, &mut errors);

    match errors.is_empty() {
        true => Ok(()),
        false => Err(errors),
    }
}

/// Validate entity consistency - ensure _Entity union members have @key directives
fn validate_entity_consistency(
    schema: &apollo_compiler::Schema,
    errors: &mut Vec<CompositionError>,
) {
    use apollo_compiler::schema::ExtendedType;
    
    // Get the _Entity union if it exists
    match schema.types.get("_Entity") {
        Some(ExtendedType::Union(entity_union)) => {
            // Check each member of the _Entity union
            for member in &entity_union.members {
                match schema.types.get(&member.name) {
                    Some(member_type) => {
                        // Verify the member type has at least one @key directive
                        let has_key = member_type
                            .directives()
                            .iter()
                            .any(|d| d.name == "key");
                        
                        match has_key {
                            false => {
                                errors.push(CompositionError::TypeDefinitionInvalid {
                                    message: format!(
                                        "Type '{}' is in _Entity union but has no @key directive",
                                        member.name
                                    ),
                                });
                            }
                            true => {} // Has @key directive
                        }
                    }
                    None => {
                        errors.push(CompositionError::TypeDefinitionInvalid {
                            message: format!(
                                "_Entity union references non-existent type '{}'",
                                member.name
                            ),
                        });
                    }
                }
            }
        }
        Some(_) => {} // Already validated that _Entity must be a union
        None => {} // No _Entity union
    }
}

/// Validate that all types are reachable from root types (Query/Mutation/Subscription)
fn validate_reachable_types(
    schema: &apollo_compiler::Schema,
    errors: &mut Vec<CompositionError>,
) {
    use std::collections::HashSet;
    use apollo_compiler::schema::ComponentName;
    
    let mut reachable_types: HashSet<ComponentName> = HashSet::new();
    let mut types_to_visit: Vec<ComponentName> = Vec::new();
    
    // Start with root types
    if let Some(query_type) = &schema.schema_definition.query {
        reachable_types.insert(query_type.clone());
        types_to_visit.push(query_type.clone());
    }
    
    if let Some(mutation_type) = &schema.schema_definition.mutation {
        reachable_types.insert(mutation_type.clone());
        types_to_visit.push(mutation_type.clone());
    }
    
    if let Some(subscription_type) = &schema.schema_definition.subscription {
        reachable_types.insert(subscription_type.clone());
        types_to_visit.push(subscription_type.clone());
    }
    
    // Traverse the type graph
    while let Some(type_name) = types_to_visit.pop() {
        match schema.types.get(type_name.as_str()) {
            Some(extended_type) => {
                collect_referenced_types(extended_type, &mut reachable_types, &mut types_to_visit);
            }
            None => {} // Type not found, skip
        }
    }
    
    // Check for orphaned types (excluding built-in types and federation types)
    for (type_name, extended_type) in schema.types.iter() {
        // Skip built-in types
        match extended_type.is_built_in() {
            true => continue,
            false => {}
        }
        
        // Skip federation-specific types
        let is_federation_type = type_name.as_str().starts_with("_") 
            || type_name.as_str().starts_with("federation__")
            || type_name.as_str().starts_with("link__");
        
        match is_federation_type {
            true => continue,
            false => {}
        }
        
        // Check if type is reachable
        match reachable_types.contains(type_name.as_str()) {
            false => {
                errors.push(CompositionError::TypeDefinitionInvalid {
                    message: format!(
                        "Type '{}' is not reachable from any root type (Query/Mutation/Subscription)",
                        type_name
                    ),
                });
            }
            true => {} // Type is reachable
        }
    }
}

/// Helper function to collect all types referenced by a given type
fn collect_referenced_types(
    extended_type: &apollo_compiler::schema::ExtendedType,
    reachable: &mut HashSet<apollo_compiler::schema::ComponentName>,
    to_visit: &mut Vec<apollo_compiler::schema::ComponentName>,
) {
    use apollo_compiler::schema::ExtendedType;
    
    match extended_type {
        ExtendedType::Object(obj_type) => {
            // Add interface implementations
            for interface in &obj_type.implements_interfaces {
                if reachable.insert(interface.clone()) {
                    to_visit.push(interface.clone());
                }
            }
            
            // Add field types
            for (_field_name, field) in obj_type.fields.iter() {
                let field_type_name = field.ty.inner_named_type().clone();
                let component_name = apollo_compiler::schema::ComponentName::from(field_type_name);
                if reachable.insert(component_name.clone()) {
                    to_visit.push(component_name);
                }
                
                // Add argument types
                for arg in &field.arguments {
                    let arg_type_name = arg.ty.inner_named_type().clone();
                    let component_name = apollo_compiler::schema::ComponentName::from(arg_type_name);
                    if reachable.insert(component_name.clone()) {
                        to_visit.push(component_name);
                    }
                }
            }
        }
        ExtendedType::Interface(interface_type) => {
            // Add interface implementations
            for interface in &interface_type.implements_interfaces {
                if reachable.insert(interface.clone()) {
                    to_visit.push(interface.clone());
                }
            }
            
            // Add field types
            for (_field_name, field) in interface_type.fields.iter() {
                let field_type_name = field.ty.inner_named_type().clone();
                let component_name = apollo_compiler::schema::ComponentName::from(field_type_name);
                if reachable.insert(component_name.clone()) {
                    to_visit.push(component_name);
                }
                
                // Add argument types
                for arg in &field.arguments {
                    let arg_type_name = arg.ty.inner_named_type().clone();
                    let component_name = apollo_compiler::schema::ComponentName::from(arg_type_name);
                    if reachable.insert(component_name.clone()) {
                        to_visit.push(component_name);
                    }
                }
            }
        }
        ExtendedType::Union(union_type) => {
            // Add union members
            for member in &union_type.members {
                let member_name = apollo_compiler::schema::ComponentName::from(member.name.clone());
                if reachable.insert(member_name.clone()) {
                    to_visit.push(member_name);
                }
            }
        }
        ExtendedType::InputObject(input_type) => {
            // Add input field types
            for (_field_name, field) in input_type.fields.iter() {
                let field_type_name = field.ty.inner_named_type().clone();
                let component_name = apollo_compiler::schema::ComponentName::from(field_type_name);
                if reachable.insert(component_name.clone()) {
                    to_visit.push(component_name);
                }
            }
        }
        ExtendedType::Enum(_) | ExtendedType::Scalar(_) => {
            // Enums and scalars don't reference other types
        }
    }
}
