# Protobuf Starlark

This page documents the typed Starlark API exposed by the built-in `protobuf-evolution` check.

## Entry Point

Policies must define:

```python
def check(ctx: ProtoContext) -> list[Finding]:
    ...
```

`ctx` exposes:

- `ctx.config: PolicyConfig`
- `ctx.parser: ParserInfo`
- `ctx.registries: list[ExtensionRegistryInfo]`
- `ctx.files: list[DescriptorPair]`
- `ctx.deltas: list[SchemaDelta]`

## Typed Enums

The API exposes enum-like globals for comparisons:

- `DeltaKinds.*`
- `Severities.*`
- `FieldKinds.*`
- `FieldLabels.*`
- `ParserBackends.*`

Examples:

```python
if delta.kind == DeltaKinds.field_removed:
    ...

if field.kind == FieldKinds.message:
    ...

return [finding(
    severity = Severities.warning,
    message = "custom warning",
    path = delta.path,
)]
```

## Descriptor Model

`DescriptorPair`:

- `path: str`
- `before: FileDescriptor | None`
- `after: FileDescriptor | None`

`FileDescriptor`:

- `path: str`
- `package: str`
- `syntax: str`
- `options: DescriptorOptions`
- `messages: list[MessageDescriptor]`
- `enums: list[EnumDescriptor]`
- `services: list[ServiceDescriptor]`
- `extensions: list[FieldDescriptor]`

`MessageDescriptor`:

- `full_name: str`
- `name: str`
- `options: DescriptorOptions`
- `is_map_entry: bool`
- `fields: list[FieldDescriptor]`
- `oneofs: list[OneofDescriptor]`
- `extensions: list[FieldDescriptor]`
- `reserved_ranges: list[ReservedRange]`
- `reserved_names: list[str]`
- `nested_messages: list[MessageDescriptor]`
- `nested_enums: list[EnumDescriptor]`

`FieldDescriptor`:

- `full_name: str`
- `name: str`
- `number: int`
- `label: FieldLabel`
- `kind: FieldKind`
- `type_name: str | None`
- `json_name: str | None`
- `oneof_index: int | None`
- `oneof_name: str | None`
- `proto3_optional: bool`
- `extendee: str | None`
- `options: DescriptorOptions`

`EnumDescriptor`:

- `full_name: str`
- `name: str`
- `options: DescriptorOptions`
- `reserved_ranges: list[ReservedRange]`
- `reserved_names: list[str]`
- `values: list[EnumValueDescriptor]`

`ServiceDescriptor`:

- `full_name: str`
- `name: str`
- `options: DescriptorOptions`
- `methods: list[MethodDescriptor]`

`MethodDescriptor`:

- `full_name: str`
- `name: str`
- `input_type: str`
- `output_type: str`
- `client_streaming: bool`
- `server_streaming: bool`
- `options: DescriptorOptions`

`DescriptorOptions`:

- `fingerprint: str`
- `has_unknown_fields: bool`
- `uninterpreted: list[UninterpretedOption]`
- `extensions: list[OptionExtension]`

`OptionExtension`:

- `registry_name: str`
- `full_name: str`
- `extendee: str`
- `field_number: int`
- `kind: FieldKind`
- `type_name: str | None`
- `is_repeated: bool`
- `values: list[str]`
- `decoded: bool`

`ExtensionRegistryInfo`:

- `name: str`
- `extension_count: int`
- `files: list[str]`
- `extendees: list[str]`

The options surface is intentionally conservative: it is meant to give policies a stable way to notice option changes and uninterpreted/custom-option presence without pretending we fully understand every extension registry.

Once `extension_registries` are configured, custom options that survive into descriptor unknown fields are resolved against those registries and exposed through `DescriptorOptions.extensions`.

## Delta Model

`SchemaDelta` includes:

- `kind: DeltaKind`
- `path: str`
- `symbol: str`

And optional detail fields such as:

- `before_kind`, `after_kind`
- `before_label`, `after_label`
- `before_number`, `after_number`
- `field_number`
- `before_package`, `after_package`
- `before_syntax`, `after_syntax`
- `before_input_type`, `after_input_type`
- `before_output_type`, `after_output_type`
- `before_oneof`, `after_oneof`
- `before_option_fingerprint`, `after_option_fingerprint`
- `before_client_streaming`, `after_client_streaming`
- `before_server_streaming`, `after_server_streaming`
- `before_map_entry`, `after_map_entry`
- `range_start`, `range_end`
- `name`

Current built-in delta kinds include:

- `message_removed`
- `enum_removed`
- `field_removed`
- `field_number_changed`
- `field_type_changed`
- `field_label_changed`
- `field_oneof_changed`
- `enum_value_removed`
- `enum_value_number_changed`
- `message_reserved_range_removed`
- `message_reserved_name_removed`
- `enum_reserved_range_removed`
- `enum_reserved_name_removed`
- `oneof_removed`
- `service_removed`
- `method_removed`
- `method_signature_changed`
- `package_changed`
- `syntax_changed`
- `map_entry_changed`
- `extension_removed`
- `extension_number_changed`
- `extension_type_changed`
- `extension_label_changed`
- `file_options_changed`
- `message_options_changed`
- `field_options_changed`
- `oneof_options_changed`
- `enum_options_changed`
- `enum_value_options_changed`
- `service_options_changed`
- `method_options_changed`
- `extension_options_changed`

## Helper Functions

Helpers exported into policy scope:

- `finding(...)`
- `error(...)`
- `warning(...)`
- `info(...)`
- `finding_for_delta(ctx, delta, message, severity=None, remediation=None)`
- `filter_deltas(ctx, kind=None, symbol_prefix=None, path=None)`
- `removed_fields(ctx)`
- `changed_field_numbers(ctx)`
- `removed_messages(ctx)`
- `removed_enums(ctx)`
- `option_changed_deltas(ctx)`

Example:

```python
def check(ctx: ProtoContext) -> list[Finding]:
    findings = []
    for delta in removed_fields(ctx):
        findings.append(finding_for_delta(
            ctx,
            delta,
            "field removal requires review: {}".format(delta.symbol),
            severity = Severities.warning,
        ))
    return findings
```

## Parsing Semantics

- `parser_backend = "auto"` prefers `protoc` and falls back to the pure Rust parser.
- `extension_registries` are configured in check config and point at proto files that declare custom options/extensions.
- The check snapshots the full repository proto tree before parsing so imports still resolve when only part of the graph changes.
- Unknown/custom options are surfaced through descriptor-option fingerprints, `has_unknown_fields`, best-effort uninterpreted-option entries, and registry-resolved `options.extensions`.
- Registry decoding is intentionally conservative. Scalar wire-compatible values are decoded to strings; message-valued extensions currently surface as hex payloads rather than fully parsed submessages.
