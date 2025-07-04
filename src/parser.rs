// Copyright (c) 2020 Ghaith Hachem and Mathias Rieder

use std::ops::Range;

use plc_ast::{
    ast::{
        AccessModifier, ArgumentProperty, AstFactory, AstNode, AstStatement, AutoDerefType, CompilationUnit,
        ConfigVariable, DataType, DataTypeDeclaration, DeclarationKind, DirectAccessType, GenericBinding,
        HardwareAccessType, Identifier, Implementation, Interface, LinkageType, PolymorphismMode, Pou,
        PouType, PropertyBlock, PropertyImplementation, PropertyKind, ReferenceAccess, ReferenceExpr,
        TypeNature, UserTypeDeclaration, Variable, VariableBlock, VariableBlockType,
    },
    provider::IdProvider,
};
use plc_diagnostics::{
    diagnostician::Diagnostician,
    diagnostics::{Diagnostic, Severity},
};
use plc_source::{
    source_location::{SourceLocation, SourceLocationFactory},
    SourceCode, SourceContainer,
};
use plc_util::convention::qualified_name;

use crate::{
    expect_token,
    lexer::{
        self, ParseSession,
        Token::{self, *},
    },
    typesystem::DINT_TYPE,
};

use self::{
    control_parser::parse_control_statement,
    expressions_parser::{parse_expression, parse_expression_list},
};

mod control_parser;
pub mod expressions_parser;

#[cfg(test)]
pub mod tests;
pub type ParsedAst = (CompilationUnit, Vec<Diagnostic>);

pub fn parse_file(
    source: &SourceCode,
    linkage: LinkageType,
    id_provider: IdProvider,
    diagnostician: &mut Diagnostician,
) -> Result<CompilationUnit, Diagnostic> {
    let location_factory = SourceLocationFactory::for_source(source);
    let (unit, errors) = parse(
        lexer::lex_with_ids(&source.source, id_provider, location_factory),
        linkage,
        source.get_location_str(),
    );
    //Register the source file with the diagnostician
    //TODO: We should reduce the clone here
    diagnostician.register_file(source.get_location_str().to_string(), source.source.clone()); // TODO: Remove clone here, generally passing the GlobalContext instead of the actual source here or in the handle method should be sufficient
    if diagnostician.handle(&errors) == Severity::Error {
        Err(Diagnostic::new("Compilation aborted due to critical parse errors").with_sub_diagnostics(errors))
    } else {
        Ok(unit)
    }
}

pub fn parse(mut lexer: ParseSession, lnk: LinkageType, file_name: &'static str) -> ParsedAst {
    let mut unit = CompilationUnit::new(file_name);

    let mut linkage = lnk;
    let mut constant = false;
    loop {
        match lexer.token {
            PropertyExternal => {
                linkage = LinkageType::External;
                lexer.advance();
                //Don't reset linkage
                continue;
            }
            PropertyConstant => {
                // parse optional const pragma (only allowed in builtins for now)
                constant = true;
                lexer.advance();
                continue;
            }
            KeywordInterface => {
                // We ignore any method implementations in interfaces as we do not support default impls yet
                let (interfaces, _) = parse_interface(&mut lexer);
                unit.interfaces.push(interfaces);
            }
            KeywordVarGlobal => unit.global_vars.push(parse_variable_block(&mut lexer, linkage)),
            KeywordVarConfig => unit.var_config.extend(parse_config_variables(&mut lexer)),

            KeywordProgram | KeywordClass | KeywordFunction | KeywordFunctionBlock => {
                let params = match lexer.token {
                    KeywordProgram => (PouType::Program, KeywordEndProgram),
                    KeywordClass => (PouType::Class, KeywordEndClass),
                    KeywordFunction => (PouType::Function, KeywordEndFunction),
                    _ => (PouType::FunctionBlock, KeywordEndFunctionBlock),
                };

                parse_pou(&mut lexer, &mut unit, params.0, linkage, params.1, constant);

                // reset const pragma
                constant = false;
            }
            KeywordAction => {
                if let Some(implementation) = parse_action(&mut lexer, linkage, None) {
                    unit.implementations.push(implementation);
                }
            }
            KeywordActions => {
                let last_pou = unit
                    .pous
                    .iter()
                    .filter(|it| {
                        // Only consider the last POU that is a program, function, function block
                        // or class
                        matches!(
                            it.kind,
                            PouType::Program | PouType::Function | PouType::FunctionBlock | PouType::Class
                        )
                    })
                    .next_back()
                    .map(|it| it.name.as_str())
                    .unwrap_or("__unknown__");
                let mut actions = parse_actions(&mut lexer, linkage, last_pou);
                unit.implementations.append(&mut actions);
            }
            KeywordType => {
                let unit_type = parse_type(&mut lexer);
                for utype in unit_type {
                    unit.user_types.push(utype);
                }
            }
            KeywordEndActions | End => return (unit, lexer.diagnostics),
            _ => {
                lexer.accept_diagnostic(Diagnostic::unexpected_token_found(
                    "StartKeyword",
                    lexer.slice(),
                    lexer.location(),
                ));
                lexer.advance();
            }
        };
        linkage = lnk;
    }
    //the match in the loop will always return
}
fn parse_actions(
    lexer: &mut ParseSession,
    linkage: LinkageType,
    default_container: &str,
) -> Vec<Implementation> {
    parse_any_in_region(lexer, vec![KeywordEndActions], |lexer| {
        lexer.advance();
        let container =
            if lexer.token == Identifier { lexer.slice_and_advance() } else { default_container.into() };
        let mut impls = vec![];

        //Go through each action
        while lexer.token != KeywordEndActions && !lexer.is_end_of_stream() {
            match lexer.token {
                KeywordAction => {
                    if let Some(implementation) = parse_action(lexer, linkage, Some(&container)) {
                        impls.push(implementation);
                    }
                }
                _ => {
                    lexer.accept_diagnostic(Diagnostic::unexpected_token_found(
                        "KeywordAction",
                        lexer.slice(),
                        lexer.location(),
                    ));
                    return impls;
                }
            }
        }
        impls
    })
}

/// Parses an interface and its methods / properties
fn parse_interface(lexer: &mut ParseSession) -> (Interface, Vec<Implementation>) {
    let location_start = lexer.range().start;
    lexer.try_consume_or_report(KeywordInterface);

    let (name, location_name) = match lexer.token {
        Token::Identifier => parse_identifier(lexer).expect("unreachable, already matched here"),

        _ => {
            lexer.accept_diagnostic(
                Diagnostic::new("Expected a name for the interface definition but got nothing")
                    .with_error_code("E006")
                    .with_location(lexer.last_location()),
            );

            // We want to keep parsing, hence we return some undefined values; the parser will yield an
            // unrecoverable error though
            (String::new(), SourceLocation::undefined())
        }
    };

    let mut extensions = Vec::new();
    let mut methods = Vec::new();
    let mut implementations = Vec::new();
    let mut properties = Vec::new();

    if lexer.try_consume(KeywordExtends) {
        while let Identifier = lexer.token {
            let (name, location) = parse_identifier(lexer).expect("unreachable, already matched here");
            extensions.push(Identifier { name, location });
            lexer.try_consume(KeywordComma);
        }
    }
    loop {
        match lexer.token {
            KeywordMethod => {
                if let Some((method, imp)) =
                    parse_method(lexer, &name, DeclarationKind::Abstract, LinkageType::Internal, false)
                {
                    // This is temporary? At some point we'll support them but for now it's a diagnostic
                    if !imp.statements.is_empty() {
                        lexer.accept_diagnostic(
                            Diagnostic::new("Interfaces can not have a default implementation")
                                .with_error_code("E113")
                                .with_location(&imp.statements.first().unwrap().location),
                        );
                    }

                    methods.push(method);
                    implementations.push(imp);
                }
            }

            KeywordProperty => {
                if let Some(property) = parse_property(lexer) {
                    for property in property.implementations.iter().filter(|imp| !imp.body.is_empty()) {
                        lexer.accept_diagnostic(
                            Diagnostic::new("Interfaces can not have a default implementation")
                                .with_error_code("E113")
                                .with_location(&property.body.first().unwrap().location),
                        );
                    }

                    properties.push(property);
                }
            }

            _ => break,
        }
    }

    lexer.try_consume_or_report(KeywordEndInterface);
    let location_end = lexer.range().start;

    (
        Interface {
            id: lexer.next_id(),
            ident: Identifier { name, location: location_name },
            methods,
            extensions,
            location: lexer.source_range_factory.create_range(location_start..location_end),
            properties,
        },
        implementations,
    )
}

///
/// parse a pou
/// # Arguments
///
/// * `lexer`       - the lexer
/// * `pou_type`    - the type of the pou currently parsed
/// * `linkage`     - internal, external ?
/// * `expected_end_token` - the token that ends this pou
///
fn parse_pou(
    lexer: &mut ParseSession,
    unit: &mut CompilationUnit,
    kind: PouType,
    linkage: LinkageType,
    expected_end_token: lexer::Token,
    constant: bool,
) {
    if constant && !matches!(linkage, LinkageType::BuiltIn) {
        lexer.accept_diagnostic(Diagnostic::const_pragma_is_not_allowed(
            lexer.last_location().span(&lexer.location()),
        ));
    }

    let start = lexer.range().start;
    lexer.advance(); //Consume ProgramKeyword
    let closing_tokens = vec![
        expected_end_token,
        KeywordEndAction,
        KeywordEndProgram,
        KeywordEndFunction,
        KeywordEndFunctionBlock,
        KeywordEndClass,
    ];
    let result = parse_any_in_region(lexer, closing_tokens.clone(), |lexer| {
        // parse polymorphism mode for all pou types
        // check in validator if pou type allows polymorphism
        let poly_mode = parse_polymorphism_mode(lexer, &kind);

        let (name, name_location) =
            parse_identifier(lexer).unwrap_or_else(|| ("".to_string(), SourceLocation::undefined())); // parse POU name

        let generics = parse_generics(lexer);

        with_scope(lexer, name.clone(), |lexer| {
            // TODO: Parse USING directives
            let super_class = parse_super_class(lexer);
            let interfaces = parse_interface_declarations(lexer);

            // parse an optional return type
            // classes do not have a return type (check in validator)
            let return_type = parse_return_type(lexer);

            // parse variable declarations. note that var in/out/inout
            // blocks are not allowed inside of class declarations.
            let mut variable_blocks = vec![];
            let allowed_var_types = [
                KeywordVar,
                KeywordVarInput,
                KeywordVarOutput,
                KeywordVarInOut,
                KeywordVarTemp,
                KeywordVarExternal,
            ];
            while allowed_var_types.contains(&lexer.token) {
                variable_blocks.push(parse_variable_block(lexer, LinkageType::Internal));
            }

            let mut impl_pous = Vec::new();
            let mut implementations = Vec::new();
            let mut properties = Vec::new();

            // classes and function blocks can have methods. methods consist of a Pou part
            // and an implementation part. That's why we get another (Pou, Implementation)
            // tuple out of parse_method() that has to be added to the list of Pous and
            // implementations. Note that function blocks have to start with the method
            // declarations before their implementation.
            // all other Pous need to be checked in the validator if they can have methods.
            while matches!(lexer.token, KeywordMethod | KeywordProperty | PropertyConstant) {
                if !matches!(kind, PouType::FunctionBlock | PouType::Class | PouType::Program) {
                    let location = lexer.source_range_factory.create_range(lexer.last_range.clone());
                    let pre = if matches!(lexer.token, KeywordProperty) { "Properties" } else { "Methods" };

                    lexer.accept_diagnostic(
                        Diagnostic::new(format!("{pre} cannot be declared in a {kind}"))
                            .with_location(location),
                    );
                }

                if lexer.token == KeywordProperty {
                    if let Some(property) = parse_property(lexer) {
                        properties.push(property);
                    }
                } else {
                    let is_const = lexer.try_consume(PropertyConstant);
                    if let Some((pou, implementation)) =
                        parse_method(lexer, &name, DeclarationKind::Concrete, linkage, is_const)
                    {
                        impl_pous.push(pou);
                        implementations.push(implementation);
                    }
                }
            }

            // a class may not contain an implementation
            // check in validator
            implementations.push(parse_implementation(
                lexer,
                linkage,
                kind.clone(),
                &name,
                &name,
                !generics.is_empty(),
                name_location.clone(),
            ));

            let mut pous = vec![Pou {
                name,
                id: lexer.next_id(),
                kind,
                variable_blocks,
                return_type,
                location: lexer.source_range_factory.create_range(start..lexer.range().end),
                name_location,
                poly_mode,
                generics,
                linkage,
                super_class,
                interfaces,
                is_const: constant,
                properties,
            }];
            pous.append(&mut impl_pous);

            (pous, implementations)
        })
    });

    //check if we ended on the right end-keyword
    if closing_tokens.contains(&lexer.last_token) && lexer.last_token != expected_end_token {
        lexer.accept_diagnostic(Diagnostic::unexpected_token_found(
            format!("{expected_end_token:?}").as_str(),
            lexer.slice_region(lexer.last_range.clone()),
            lexer.source_range_factory.create_range(lexer.last_range.clone()),
        ));
    }

    let (mut pous, mut implementations) = result;
    unit.pous.append(&mut pous);
    unit.implementations.append(&mut implementations);
}

fn parse_generics(lexer: &mut ParseSession) -> Vec<GenericBinding> {
    if lexer.try_consume(Token::OperatorLess) {
        parse_any_in_region(lexer, vec![Token::OperatorGreater], |lexer| {
            let mut generics = vec![];
            loop {
                //identifier
                if let Some((name, _)) = parse_identifier(lexer) {
                    lexer.try_consume_or_report(Token::KeywordColon);

                    //Expect a type nature
                    if let Some(nature) = parse_identifier(lexer).map(|(it, _)| parse_type_nature(lexer, &it))
                    {
                        generics.push(GenericBinding { name, nature });
                    }
                }

                if !lexer.try_consume(Token::KeywordComma) || lexer.try_consume(Token::OperatorGreater) {
                    break;
                }
            }

            generics
        })
    } else {
        vec![]
    }
}

/// Parses the comma seperated identifiers after an `IMPLEMENTS` keyword, e.g. `bar` and `baz` in
/// `INTERFACE foo IMPLEMENTS bar`
fn parse_interface_declarations(lexer: &mut ParseSession) -> Vec<Identifier> {
    let mut declarations = Vec::new();

    if !lexer.try_consume(KeywordImplements) {
        return declarations;
    }

    if lexer.token != Token::Identifier {
        lexer.accept_diagnostic(
            Diagnostic::new(
                "Expected a comma separated list of identifiers after `IMPLEMENTS` but got nothing",
            )
            .with_error_code("E006")
            .with_location(lexer.last_location()),
        );

        return declarations;
    }

    loop {
        match lexer.token {
            Token::Identifier => {
                let (name, location) = parse_identifier(lexer).expect("Identifier already matched");
                declarations.push(Identifier { name, location });
            }
            Token::KeywordComma => lexer.advance(),

            _ => break,
        }
    }

    declarations
}

fn parse_type_nature(lexer: &mut ParseSession, nature: &str) -> TypeNature {
    match nature {
        "ANY" => TypeNature::Any,
        "ANY_DERIVED" => TypeNature::Derived,
        "ANY_ELEMENTARY" => TypeNature::Elementary,
        "ANY_MAGNITUDE" => TypeNature::Magnitude,
        "ANY_NUM" => TypeNature::Num,
        "ANY_REAL" => TypeNature::Real,
        "ANY_INT" => TypeNature::Int,
        "ANY_SIGNED" => TypeNature::Signed,
        "ANY_UNSIGNED" => TypeNature::Unsigned,
        "ANY_DURATION" => TypeNature::Duration,
        "ANY_BIT" => TypeNature::Bit,
        "ANY_CHARS" => TypeNature::Chars,
        "ANY_STRING" => TypeNature::String,
        "ANY_CHAR" => TypeNature::Char,
        "ANY_DATE" => TypeNature::Date,
        "__ANY_VLA" => TypeNature::__VLA,
        _ => {
            lexer.accept_diagnostic(
                Diagnostic::new(format!("Unkown type nature `{nature}`"))
                    .with_location(lexer.location())
                    .with_error_code("E063"),
            );
            TypeNature::Any
        }
    }
}

fn parse_polymorphism_mode(lexer: &mut ParseSession, pou_type: &PouType) -> Option<PolymorphismMode> {
    match pou_type {
        PouType::Class | PouType::FunctionBlock | PouType::Method { .. } => {
            Some(
                // See if the method/pou was declared FINAL or ABSTRACT
                if lexer.try_consume(KeywordFinal) {
                    PolymorphismMode::Final
                } else if lexer.try_consume(KeywordAbstract) {
                    PolymorphismMode::Abstract
                } else {
                    PolymorphismMode::None
                },
            )
        }
        _ => None,
    }
}

fn parse_super_class(lexer: &mut ParseSession) -> Option<Identifier> {
    let mut extensions = vec![];
    while lexer.try_consume(KeywordExtends) {
        let name_and_location = parse_identifier(lexer)?;
        extensions.push(name_and_location);
    }
    extensions.iter().skip(1).for_each(|(_, location)| {
        lexer.accept_diagnostic(
            Diagnostic::new("Multiple inheritance. POUs can only be extended once.".to_string())
                .with_error_code("E114")
                .with_location(location),
        )
    });

    extensions
        .first()
        .map(|(name, location)| Identifier { name: name.to_string(), location: location.clone() })
}

fn parse_return_type(lexer: &mut ParseSession) -> Option<DataTypeDeclaration> {
    if lexer.try_consume(KeywordColon) {
        if let Some((declaration, initializer)) = parse_data_type_definition(lexer, None) {
            if let Some(init) = initializer {
                lexer.accept_diagnostic(
                    Diagnostic::new("Return types cannot have a default value, the value will be ignored")
                        .with_location(init.get_location())
                        .with_error_code("E016"),
                );
            }

            if let DataTypeDeclaration::Definition { data_type, .. } = &declaration {
                if matches!(data_type, DataType::EnumType { .. } | DataType::StructType { .. }) {
                    let datatype_name = declaration
                        .get_location()
                        .to_range()
                        .map(|range| &lexer.get_src()[range])
                        .expect("Expecing location to be a range during parsing");
                    lexer.accept_diagnostic(
                        ////TODO: This prints a debug version of the datatype, it should have a user readable version instead
                        Diagnostic::new(format!(
                            "Data Type {datatype_name} not supported as a function return type!"
                        ))
                        .with_error_code("E027")
                        .with_location(&declaration),
                    )
                }
            }

            Some(declaration)
        } else {
            //missing return type
            lexer.accept_diagnostic(Diagnostic::unexpected_token_found(
                "Datatype",
                lexer.slice(),
                lexer.source_range_factory.create_range(lexer.range()),
            ));
            None
        }
    } else {
        None
    }
}

fn parse_method(
    lexer: &mut ParseSession,
    parent: &str,
    declaration_kind: DeclarationKind,
    linkage: LinkageType,
    constant: bool,
) -> Option<(Pou, Implementation)> {
    parse_any_in_region(lexer, vec![KeywordEndMethod], |lexer| {
        // Method declarations look like this:
        // METHOD [AccessModifier] [ABSTRACT|FINAL] [OVERRIDE] [: return_type]
        //    ...
        // END_METHOD

        // constant pragma is only allowed in builtins for now
        if constant {
            lexer.accept_diagnostic(Diagnostic::const_pragma_is_not_allowed(
                lexer.last_location().span(&lexer.location()),
            ));
        }

        let method_start = lexer.range().start;
        lexer.advance(); // eat METHOD keyword

        let access = Some(parse_access_modifier(lexer));
        let pou_kind = PouType::Method { parent: parent.into(), property: None, declaration_kind };
        let poly_mode = parse_polymorphism_mode(lexer, &pou_kind);
        let overriding = lexer.try_consume(KeywordOverride);
        let (name, name_location) = parse_identifier(lexer)?;
        let generics = parse_generics(lexer);
        let return_type = parse_return_type(lexer);

        let mut variable_blocks = vec![];
        while lexer.token == KeywordVar
            || lexer.token == KeywordVarInput
            || lexer.token == KeywordVarOutput
            || lexer.token == KeywordVarInOut
            || lexer.token == KeywordVarTemp
        {
            variable_blocks.push(parse_variable_block(lexer, LinkageType::Internal));
        }

        let call_name = qualified_name(parent, &name);
        let implementation = parse_implementation(
            lexer,
            linkage,
            pou_kind.clone(),
            &call_name,
            &call_name,
            !generics.is_empty(),
            name_location.clone(),
        );

        // parse_implementation() will default-initialize the fields it
        // doesn't know. thus, we have to complete the information.
        let implementation = Implementation { overriding, access, ..implementation };

        let method_end = lexer.range().end;
        Some((
            Pou {
                name: call_name,
                id: lexer.next_id(),
                kind: pou_kind,
                variable_blocks,
                return_type,
                location: lexer.source_range_factory.create_range(method_start..method_end),
                name_location,
                poly_mode,
                generics,
                linkage,
                super_class: None,
                interfaces: Vec::new(),
                properties: Vec::new(),
                is_const: constant,
            },
            implementation,
        ))
    })
}

fn parse_property(lexer: &mut ParseSession) -> Option<PropertyBlock> {
    lexer.advance(); // Move past `PROPERTY` keyword

    let mut has_error = false;

    let identifier = parse_identifier(lexer);
    if identifier.is_none() {
        has_error = true;
        lexer.accept_diagnostic(
            Diagnostic::new("Property definition is missing a name").with_location(lexer.location()),
        );
    }

    let datatype = parse_return_type(lexer);
    if datatype.is_none() {
        has_error = true;
        lexer.accept_diagnostic(
            Diagnostic::new("Property definition is missing a datatype").with_location(lexer.last_location()),
        );
    };

    // This is kind of common, hence we parse invalid variable blocks to have useful error messages
    while lexer.token.is_var() {
        let block = parse_variable_block(lexer, LinkageType::Internal);
        lexer.accept_diagnostic(
            Diagnostic::new(
                "Variable blocks may only be defined within a GET or SET block in the context of properties",
            )
            .with_location(&block.location)
            .with_error_code("E007"),
        );
    }

    let mut implementations = Vec::new();
    while matches!(lexer.token, KeywordGet | KeywordSet) {
        let location = lexer.location();
        let kind = if lexer.token == KeywordGet { PropertyKind::Get } else { PropertyKind::Set };
        lexer.advance(); // Move past `GET` or `SET` keyword

        let mut variable_blocks = Vec::new();
        while lexer.token.is_var() {
            variable_blocks.push(parse_variable_block(lexer, LinkageType::Internal));
        }

        let statements = parse_body_in_region(
            lexer,
            match kind {
                PropertyKind::Get => vec![Token::KeywordEndGet],
                PropertyKind::Set => vec![Token::KeywordEndSet],
            },
        );
        implementations.push(PropertyImplementation {
            kind,
            variable_blocks,
            body: statements,
            location,
            end_location: lexer.last_location(),
        });
    }

    lexer.try_consume_or_report(Token::KeywordEndProperty); // Move past `END_PROPERTY` keyword

    if has_error {
        return None;
    };

    let (name, name_location) = identifier.expect("covered above");
    let datatype = datatype.expect("covered above");
    Some(PropertyBlock { ident: Identifier { name, location: name_location }, datatype, implementations })
}

fn parse_access_modifier(lexer: &mut ParseSession) -> AccessModifier {
    if lexer.try_consume(KeywordAccessPublic) {
        AccessModifier::Public
    } else if lexer.try_consume(KeywordAccessPrivate) {
        AccessModifier::Private
    } else if lexer.try_consume(KeywordAccessProtected) {
        AccessModifier::Protected
    } else if lexer.try_consume(KeywordAccessInternal) {
        AccessModifier::Internal
    } else {
        AccessModifier::Protected
    }
}

/// parse identifier and advance if successful
/// returns the identifier as a String and the SourceRange of the parsed name
fn parse_identifier(lexer: &mut ParseSession) -> Option<(String, SourceLocation)> {
    let pou_name = lexer.slice().to_string();
    if lexer.token == Identifier {
        lexer.advance();
        Some((pou_name, lexer.last_location()))
    } else {
        lexer.accept_diagnostic(Diagnostic::unexpected_token_found(
            "Identifier",
            pou_name.as_str(),
            lexer.location(),
        ));
        None
    }
}

fn parse_implementation(
    lexer: &mut ParseSession,
    linkage: LinkageType,
    pou_type: PouType,
    call_name: &str,
    type_name: &str,
    generic: bool,
    name_location: SourceLocation,
) -> Implementation {
    let start = lexer.range().start;
    let statements = parse_body_standalone(lexer);
    let end_location = lexer.location(); //Location of the current token, which shoudl be the
                                         //end token
    Implementation {
        name: call_name.into(),
        type_name: type_name.into(),
        linkage,
        pou_type,
        statements,
        location: lexer.source_range_factory.create_range(start..lexer.last_range.end),
        name_location,
        end_location,
        overriding: false,
        generic,
        access: None,
    }
}

fn parse_action(
    lexer: &mut ParseSession,
    linkage: LinkageType,
    container: Option<&str>,
) -> Option<Implementation> {
    lexer.advance(); //Consume the Action keyword
    let closing_tokens =
        vec![KeywordEndAction, KeywordEndProgram, KeywordEndFunction, KeywordEndFunctionBlock];

    parse_any_in_region(lexer, closing_tokens.clone(), |lexer| {
        let name_or_container = lexer.slice_and_advance();

        let (container, name, name_location) = if let Some(container) = container {
            (container.into(), name_or_container, lexer.last_location())
        } else {
            let loc = lexer.last_location();
            expect_token!(lexer, KeywordDot, None);

            lexer.advance();

            expect_token!(lexer, Identifier, None);

            let name = lexer.slice_and_advance();
            (name_or_container, name, loc.span(&lexer.last_location()))
        };
        let call_name = qualified_name(&container, &name);

        let implementation = parse_implementation(
            lexer,
            linkage,
            PouType::Action,
            &call_name,
            &container,
            false,
            name_location,
        );
        //lets see if we ended on the right END_ keyword
        if closing_tokens.contains(&lexer.last_token) && lexer.last_token != KeywordEndAction {
            lexer.accept_diagnostic(Diagnostic::unexpected_token_found(
                format!("{KeywordEndAction:?}").as_str(),
                lexer.slice(),
                lexer.location(),
            ))
        }
        Some(implementation)
    })
}

// TYPE ... END_TYPE
fn parse_type(lexer: &mut ParseSession) -> Vec<UserTypeDeclaration> {
    lexer.advance(); // consume the TYPE

    parse_any_in_region(lexer, vec![KeywordEndType], |lexer| {
        let mut declarations = vec![];
        while !lexer.closes_open_region(&lexer.token) {
            let name = lexer.slice_and_advance();
            let name_location = lexer.last_location();
            lexer.try_consume_or_report(KeywordColon);

            let result = parse_full_data_type_definition(lexer, Some(name));

            if let Some((DataTypeDeclaration::Definition { data_type, .. }, initializer)) = result {
                declarations.push(UserTypeDeclaration {
                    data_type,
                    initializer,
                    location: name_location,
                    scope: lexer.scope.clone(),
                });
            }
        }
        declarations
    })
}

type DataTypeWithInitializer = (DataTypeDeclaration, Option<AstNode>);

fn parse_full_data_type_definition(
    lexer: &mut ParseSession,
    name: Option<String>,
) -> Option<DataTypeWithInitializer> {
    let end_keyword = if lexer.token == KeywordStruct { KeywordEndStruct } else { KeywordSemicolon };
    let parsed_datatype = parse_any_in_region(lexer, vec![end_keyword], |lexer| {
        let sized = lexer.try_consume(PropertySized);
        if lexer.try_consume(KeywordDotDotDot) {
            Some((
                DataTypeDeclaration::Definition {
                    data_type: DataType::VarArgs { referenced_type: None, sized },
                    location: lexer.last_location(),
                    scope: lexer.scope.clone(),
                },
                None,
            ))
        } else {
            parse_data_type_definition(lexer, name).map(|(type_def, initializer)| {
                if lexer.try_consume(KeywordDotDotDot) {
                    (
                        DataTypeDeclaration::Definition {
                            data_type: DataType::VarArgs { referenced_type: Some(Box::new(type_def)), sized },
                            location: lexer.last_location(),
                            scope: lexer.scope.clone(),
                        },
                        None,
                    )
                } else {
                    (type_def, initializer)
                }
            })
        }
    });

    // The standard allows semicolons at the end of an `END_STRUCT` keyword, hence if we parsed
    // a struct, try to also consume a semicolon if it exists
    if end_keyword == KeywordEndStruct {
        lexer.try_consume(KeywordSemicolon);
    }

    parsed_datatype
}

// TYPE xxx : 'STRUCT' | '(' | IDENTIFIER
fn parse_data_type_definition(
    lexer: &mut ParseSession,
    name: Option<String>,
) -> Option<DataTypeWithInitializer> {
    let start = lexer.location();
    if lexer.try_consume(KeywordStruct) {
        // Parse struct
        let variables = parse_variable_list(lexer);
        Some((
            DataTypeDeclaration::Definition {
                data_type: DataType::StructType { name, variables },
                location: start.span(&lexer.location()),
                scope: lexer.scope.clone(),
            },
            None,
        ))
    } else if lexer.try_consume(KeywordArray) {
        parse_array_type_definition(lexer, name)
    } else if lexer.try_consume(KeywordPointer) {
        let start_pos = lexer.last_range.start;
        //Report wrong keyword
        lexer.accept_diagnostic(
            Diagnostic::new("`POINTER TO` is type-unsafe, consider using `REF_TO` instead")
                .with_location(lexer.last_location())
                .with_error_code("E015"),
        );
        let expect_keyword_to = |lexer: &mut ParseSession| {
            expect_token!(lexer, KeywordTo, None);
            Some(())
        };
        if expect_keyword_to(lexer).is_some() {
            lexer.advance();
        }

        parse_pointer_definition(lexer, name, start_pos, None, false)
    } else if lexer.try_consume(KeywordRef) {
        parse_pointer_definition(lexer, name, lexer.last_range.start, None, true)
    } else if lexer.try_consume(KeywordParensOpen) {
        //enum without datatype
        parse_enum_type_definition(lexer, name)
    } else if lexer.token == KeywordString || lexer.token == KeywordWideString {
        parse_string_type_definition(lexer, name)
    } else if lexer.token == Identifier {
        parse_type_reference_type_definition(lexer, name)
    } else {
        //no datatype?
        lexer.accept_diagnostic(Diagnostic::unexpected_token_found(
            "DataTypeDefinition",
            format!("{:?}", lexer.token).as_str(),
            lexer.location(),
        ));
        None
    }
}

fn parse_pointer_definition(
    lexer: &mut ParseSession,
    name: Option<String>,
    start_pos: usize,
    auto_deref: Option<AutoDerefType>,
    type_safe: bool,
) -> Option<(DataTypeDeclaration, Option<AstNode>)> {
    parse_data_type_definition(lexer, None).map(|(decl, initializer)| {
        (
            DataTypeDeclaration::Definition {
                data_type: DataType::PointerType {
                    name,
                    referenced_type: Box::new(decl),
                    auto_deref,
                    type_safe,
                },
                // FIXME: this currently includes the initializer in the sourcelocation, resulting in 'REF_TO A := B' when creating a slice
                location: lexer.source_range_factory.create_range(start_pos..lexer.last_range.end),
                scope: lexer.scope.clone(),
            },
            initializer,
        )
    })
}

fn parse_type_reference_type_definition(
    lexer: &mut ParseSession,
    name: Option<String>,
) -> Option<(DataTypeDeclaration, Option<AstNode>)> {
    let start = lexer.range().start;
    //Subrange
    let referenced_type = lexer.slice_and_advance();

    let bounds = if lexer.try_consume(KeywordParensOpen) {
        // INT (..) :=
        let bounds = parse_expression(lexer);
        expect_token!(lexer, KeywordParensClose, None);
        lexer.advance();
        Some(bounds)
    } else {
        None
    };

    let end = lexer.last_range.end;

    let initial_value: Option<AstNode> =
        if lexer.try_consume(KeywordAssignment) || lexer.try_consume(KeywordReferenceAssignment) {
            Some(parse_expression(lexer))
        } else {
            None
        };

    if name.is_some() || bounds.is_some() {
        let data_type = match bounds {
            Some(AstNode { stmt: AstStatement::ExpressionList(expressions), id, location, .. }) => {
                //this is an enum
                DataTypeDeclaration::Definition {
                    data_type: DataType::EnumType {
                        name,
                        numeric_type: referenced_type,
                        elements: AstFactory::create_expression_list(expressions, location, id),
                    },
                    location: lexer.source_range_factory.create_range(start..end),
                    scope: lexer.scope.clone(),
                }
            }
            Some(AstNode {
                stmt: AstStatement::ReferenceExpr(ReferenceExpr { access: ReferenceAccess::Member(_), .. }),
                ..
            }) => {
                // a enum with just one element
                DataTypeDeclaration::Definition {
                    data_type: DataType::EnumType {
                        name,
                        numeric_type: referenced_type,
                        elements: bounds.unwrap(),
                    },
                    location: lexer.source_range_factory.create_range(start..end),
                    scope: lexer.scope.clone(),
                }
            }
            _ => DataTypeDeclaration::Definition {
                //something else inside the brackets -> probably a subrange?
                data_type: DataType::SubRangeType { name, referenced_type, bounds },
                location: lexer.source_range_factory.create_range(start..end),
                scope: lexer.scope.clone(),
            },
        };
        Some((data_type, initial_value))
    } else {
        Some((
            DataTypeDeclaration::Reference {
                referenced_type,
                location: lexer.source_range_factory.create_range(start..end),
            },
            initial_value,
        ))
    }
}

fn parse_string_size_expression(lexer: &mut ParseSession) -> Option<AstNode> {
    let opening_token = lexer.token;
    if lexer.try_consume(KeywordSquareParensOpen) || lexer.try_consume(KeywordParensOpen) {
        let opening_location = lexer.range().start;
        let closing_tokens = vec![KeywordSquareParensClose, KeywordParensClose];
        parse_any_in_region(lexer, closing_tokens, |lexer| {
            let size_expr = parse_expression(lexer);
            let error_range = lexer.source_range_factory.create_range(opening_location..lexer.range().end);

            if (opening_token == KeywordParensOpen && lexer.token == KeywordSquareParensClose)
                || (opening_token == KeywordSquareParensOpen && lexer.token == KeywordParensClose)
            {
                lexer.accept_diagnostic(
                    Diagnostic::new("Mismatched types of parentheses around string size expression")
                        .with_location(error_range)
                        .with_error_code("E009"),
                );
            } else if opening_token == KeywordParensOpen || lexer.token == KeywordParensClose {
                lexer.accept_diagnostic(Diagnostic::new(
                    "Unusual type of parentheses around string size expression, consider using square parentheses '[]'").
                    with_location(error_range)
                    .with_error_code("E014")
                );
            }

            Some(size_expr)
        })
    } else {
        None
    }
}

fn parse_string_type_definition(
    lexer: &mut ParseSession,
    name: Option<String>,
) -> Option<(DataTypeDeclaration, Option<AstNode>)> {
    let text = lexer.slice().to_string();
    let start = lexer.range().start;
    let is_wide = lexer.token == KeywordWideString;
    lexer.advance();

    let size = parse_string_size_expression(lexer);
    let end = lexer.last_range.end;
    let location = lexer.source_range_factory.create_range(start..end);

    match (size, &name) {
        (Some(size), _) => Some(DataTypeDeclaration::Definition {
            data_type: DataType::StringType { name, is_wide, size: Some(size) },
            location,
            scope: lexer.scope.clone(),
        }),
        (None, Some(name)) => Some(DataTypeDeclaration::Definition {
            data_type: DataType::SubRangeType {
                name: Some(name.into()),
                referenced_type: text,
                bounds: None,
            },
            location,
            scope: lexer.scope.clone(),
        }),
        _ => Some(DataTypeDeclaration::Reference { referenced_type: text, location }),
    }
    .zip(Some(
        (lexer.try_consume(KeywordAssignment) || lexer.try_consume(KeywordReferenceAssignment))
            .then(|| parse_expression(lexer)),
    ))
}

fn parse_enum_type_definition(
    lexer: &mut ParseSession,
    name: Option<String>,
) -> Option<(DataTypeDeclaration, Option<AstNode>)> {
    let start = lexer.last_location();
    let elements = parse_any_in_region(lexer, vec![KeywordParensClose], |lexer| {
        // Parse Enum - we expect at least one element
        let elements = parse_expression_list(lexer);
        Some(elements)
    })?;
    let initializer = lexer.try_consume(KeywordAssignment).then(|| parse_expression(lexer));
    Some((
        DataTypeDeclaration::Definition {
            data_type: DataType::EnumType { name, elements, numeric_type: DINT_TYPE.to_string() },
            location: start.span(&lexer.last_location()),
            scope: lexer.scope.clone(),
        },
        initializer,
    ))
}

fn parse_array_type_definition(
    lexer: &mut ParseSession,
    name: Option<String>,
) -> Option<(DataTypeDeclaration, Option<AstNode>)> {
    let start = lexer.last_range.start;
    let range = parse_any_in_region(lexer, vec![KeywordOf], |lexer| {
        // Parse Array range

        expect_token!(lexer, KeywordSquareParensOpen, None);
        lexer.advance();

        let range_statement = parse_expression(lexer);

        expect_token!(lexer, KeywordSquareParensClose, None);
        lexer.advance();

        Some(range_statement)
    })?;

    let inner_type_defintion = parse_data_type_definition(lexer, None);
    inner_type_defintion.map(|(reference, initializer)| {
        let reference_end = reference.get_location().to_range().map(|it| it.end).unwrap_or(0);
        let location = lexer.source_range_factory.create_range(start..reference_end);

        let is_variable_length = match &range.get_stmt() {
            // Single dimensions, i.e. ARRAY[0..5] or ARRAY[*]
            AstStatement::RangeStatement { .. } => Some(false),
            AstStatement::VlaRangeStatement => Some(true),

            // Multi dimensions, i.e. ARRAY [0..5, 5..10] or ARRAY [*, *]
            AstStatement::ExpressionList(expressions) => match expressions[0].get_stmt() {
                AstStatement::RangeStatement(..) => Some(false),
                AstStatement::VlaRangeStatement => Some(true),
                _ => None,
            },

            _ => None,
        };

        let is_variable_length = match is_variable_length {
            Some(val) => val,
            None => {
                lexer.accept_diagnostic(
                    Diagnostic::new(format!("Expected a range statement, got {range:?} instead"))
                        .with_location(range.get_location())
                        .with_error_code("E008"),
                );
                false
            }
        };

        (
            DataTypeDeclaration::Definition {
                data_type: DataType::ArrayType {
                    name,
                    bounds: range,
                    referenced_type: Box::new(reference),
                    is_variable_length,
                },
                location,
                scope: lexer.scope.clone(),
            },
            initializer,
        )
    })
}

/// parse a body and recovers until the given `end_keywords`
fn parse_body_in_region(lexer: &mut ParseSession, end_keywords: Vec<Token>) -> Vec<AstNode> {
    parse_any_in_region(lexer, end_keywords, parse_body_standalone)
}

fn parse_body_standalone(lexer: &mut ParseSession) -> Vec<AstNode> {
    let mut statements = Vec::new();
    while !lexer.closes_open_region(&lexer.token) {
        statements.push(parse_control(lexer));
    }
    statements
}

/// parses a statement ending with a ';'
fn parse_statement(lexer: &mut ParseSession) -> AstNode {
    let result = parse_any_in_region(lexer, vec![KeywordSemicolon, KeywordColon], parse_expression);
    if lexer.last_token == KeywordColon {
        let location = result.location.span(&lexer.last_location());
        AstFactory::create_case_condition(result, location, lexer.next_id())
    } else {
        result
    }
}

pub fn with_scope<T, F: FnOnce(&mut ParseSession) -> T>(
    lexer: &mut ParseSession,
    scope: String,
    parse_fn: F,
) -> T {
    lexer.scope = Some(scope);
    let result = parse_fn(lexer);
    lexer.scope = None;
    result
}

pub fn parse_any_in_region<T, F: FnOnce(&mut ParseSession) -> T>(
    lexer: &mut ParseSession,
    closing_tokens: Vec<Token>,
    parse_fn: F,
) -> T {
    lexer.enter_region(closing_tokens);
    let result = parse_fn(lexer);

    // try to recover by eating everything until
    // we believe the parser is able to continue
    lexer.recover_until_close();
    lexer.close_region();

    result
}

fn parse_reference(lexer: &mut ParseSession) -> AstNode {
    if let Some(statement) = expressions_parser::parse_call_statement(lexer) {
        statement
    } else {
        let statement = AstFactory::create_empty_statement(
            lexer.diagnostics.last().map_or(SourceLocation::undefined(), |d| d.get_location()),
            lexer.next_id(),
        );
        statement
    }
}

fn parse_control(lexer: &mut ParseSession) -> AstNode {
    parse_control_statement(lexer)
}

fn parse_variable_block_type(lexer: &mut ParseSession) -> VariableBlockType {
    let block_type = lexer.token;
    //Consume the type token
    lexer.advance();
    let argument_property = if lexer.try_consume(PropertyByRef) {
        //Report a diagnostic if blocktype is incompatible
        if !matches!(block_type, KeywordVarInput) {
            lexer.accept_diagnostic(
                Diagnostic::new("Invalid pragma location: Only VAR_INPUT support by ref properties")
                    .with_error_code("E024")
                    .with_location(lexer.location()),
            )
        }
        ArgumentProperty::ByRef
    } else {
        ArgumentProperty::ByVal
    };
    match block_type {
        KeywordVar => VariableBlockType::Local,
        KeywordVarTemp => VariableBlockType::Temp,
        KeywordVarInput => VariableBlockType::Input(argument_property),
        KeywordVarOutput => VariableBlockType::Output,
        KeywordVarGlobal => VariableBlockType::Global,
        KeywordVarInOut => VariableBlockType::InOut,
        KeywordVarExternal => VariableBlockType::External,
        _ => VariableBlockType::Local,
    }
}

fn parse_variable_block(lexer: &mut ParseSession, linkage: LinkageType) -> VariableBlock {
    let location = lexer.location();
    let variable_block_type = parse_variable_block_type(lexer);

    let constant = lexer.try_consume(KeywordConstant);

    let retain = lexer.try_consume(KeywordRetain);
    lexer.try_consume(KeywordNonRetain);

    let access = parse_access_modifier(lexer);

    let mut variables = parse_any_in_region(lexer, vec![KeywordEndVar], parse_variable_list);

    if constant && !matches!(variable_block_type, VariableBlockType::External) {
        // sneak in the DefaultValue-Statements if no initializers were defined
        variables.iter_mut().filter(|it| it.initializer.is_none()).for_each(|it| {
            it.initializer = Some(AstFactory::create_default_value(it.location.clone(), lexer.next_id()));
        });
    }

    VariableBlock { access, constant, retain, variables, kind: variable_block_type, linkage, location }
}

fn parse_variable_list(lexer: &mut ParseSession) -> Vec<Variable> {
    let mut variables = vec![];
    while lexer.token == Identifier {
        let mut line_vars = parse_variable_line(lexer);
        variables.append(&mut line_vars);
    }
    variables
}

fn parse_config_variables(lexer: &mut ParseSession) -> Vec<ConfigVariable> {
    parse_any_in_region(lexer, vec![KeywordEndVar], |lexer| {
        lexer.advance();
        let mut variables = vec![];
        while lexer.token == Identifier {
            if let Some(configured_var) =
                parse_any_in_region(lexer, vec![KeywordSemicolon], try_parse_config_var)
            {
                variables.push(configured_var);
            }
        }
        variables
    })
}

fn try_parse_config_var(lexer: &mut ParseSession) -> Option<ConfigVariable> {
    let start = lexer.location();
    let qualified_reference = parse_reference(lexer);
    let location = start.span(&lexer.last_location());
    if !lexer.try_consume(KeywordAt) {
        lexer.accept_diagnostic(Diagnostic::missing_token("AT", lexer.location()));
    }

    let HardwareAccess((direction, access_type)) = lexer.token else {
        lexer.accept_diagnostic(Diagnostic::missing_token("hardware access", lexer.location()));
        return None;
    };

    let address = parse_hardware_access(lexer, direction, access_type)?;

    if !lexer.try_consume(KeywordColon) {
        lexer.accept_diagnostic(Diagnostic::missing_token(
            format!("{KeywordColon:?}").as_str(),
            lexer.location(),
        ));
    }

    parse_data_type_definition(lexer, None).map(|(dt, init)| {
        if init.is_some() {
            lexer.accept_diagnostic(Diagnostic::unexpected_token_found(
                format!("{KeywordSemicolon:?}").as_str(),
                "Initializer",
                lexer.last_location().span(&lexer.location()),
            ))
        }
        ConfigVariable::new(qualified_reference, dt, address, location)
    })
}

fn parse_aliasing(lexer: &mut ParseSession, names: &(String, Range<usize>)) -> Option<Variable> {
    let reference = parse_reference(lexer);
    if !lexer.try_consume(KeywordColon) {
        lexer.accept_diagnostic(Diagnostic::missing_token(
            format!("{KeywordColon:?}").as_str(),
            lexer.location(),
        ));
    }

    let start = &lexer.location().get_span().to_range().unwrap_or(lexer.last_range.clone()).start;
    let datatype = parse_pointer_definition(lexer, None, *start, Some(AutoDerefType::Alias), true);
    if !lexer.try_consume(KeywordSemicolon) {
        lexer.accept_diagnostic(Diagnostic::missing_token(
            format!("{KeywordSemicolon:?}").as_str(),
            lexer.location(),
        ));
    }

    if let Some((data_type, _)) = datatype {
        return Some(Variable {
            name: names.0.clone(),
            data_type_declaration: data_type,
            location: lexer.source_range_factory.create_range(names.1.clone()),
            initializer: Some(reference),
            address: None,
        });
    }

    None
}

fn parse_variable_line(lexer: &mut ParseSession) -> Vec<Variable> {
    // read in a comma separated list of variable names
    let mut var_names: Vec<(String, Range<usize>)> = vec![];
    while lexer.token == Identifier {
        let location = lexer.range();
        let identifier_end = location.end;
        var_names.push((lexer.slice_and_advance(), location));

        if lexer.token == KeywordColon || lexer.token == KeywordAt {
            break;
        }

        if !lexer.try_consume(KeywordComma) {
            let next_token_start = lexer.range().start;
            lexer.accept_diagnostic(Diagnostic::missing_token(
                format!("{KeywordColon:?} or {KeywordComma:?}").as_str(),
                lexer.source_range_factory.create_range(identifier_end..next_token_start),
            ));
        }
    }

    //See if there's an AT keyword
    let mut address: Option<AstNode> = None;
    if lexer.try_consume(KeywordAt) {
        match lexer.token {
            HardwareAccess((direction, access_type)) => {
                address = parse_hardware_access(lexer, direction, access_type)
            }

            Identifier => {
                return match parse_aliasing(lexer, &var_names[0]) {
                    Some(aliased_variable) => vec![aliased_variable],
                    None => vec![],
                };
            }

            _ => {
                lexer.accept_diagnostic(Diagnostic::missing_token(
                    "hardware access or identifier",
                    lexer.location(),
                ));
            }
        }
    };

    // colon has to come before the data type
    if !lexer.try_consume(KeywordColon) {
        lexer.accept_diagnostic(Diagnostic::missing_token(
            format!("{KeywordColon:?}").as_str(),
            lexer.location(),
        ));
    }

    // create variables with the same data type for each of the names
    let mut variables = vec![];

    let parse_definition_opt = if lexer.try_consume(KeywordReferenceTo) {
        parse_pointer_definition(lexer, None, lexer.last_range.start, Some(AutoDerefType::Reference), true)
    } else if address.is_some() {
        parse_pointer_definition(lexer, None, lexer.last_range.start, Some(AutoDerefType::Alias), true)
    } else {
        parse_full_data_type_definition(lexer, None)
    };

    lexer.try_consume(KeywordSemicolon);

    if let Some((data_type, initializer)) = parse_definition_opt {
        for (name, range) in var_names {
            variables.push(Variable {
                name,
                data_type_declaration: data_type.clone(),
                location: lexer.source_range_factory.create_range(range),
                initializer: initializer.clone(),
                address: address.clone(),
            });
        }
    }

    variables
}

fn parse_hardware_access(
    lexer: &mut ParseSession,
    hardware_access_type: HardwareAccessType,
    access_type: DirectAccessType,
) -> Option<AstNode> {
    let start_location = lexer.last_location();
    lexer.advance();
    //Folowed by an integer
    if access_type == DirectAccessType::Template || lexer.token == LiteralInteger {
        let mut address = vec![];
        if lexer.token == LiteralInteger {
            loop {
                let int = expressions_parser::parse_strict_literal_integer(lexer)?;
                address.push(int);
                if !lexer.try_consume(KeywordDot) {
                    break;
                }
            }
        }
        Some(AstFactory::create_hardware_access(
            access_type,
            hardware_access_type,
            address,
            start_location.span(&lexer.last_location()),
            lexer.next_id(),
        ))
    } else {
        lexer.accept_diagnostic(Diagnostic::missing_token("LiteralInteger", lexer.location()));
        None
    }
}
