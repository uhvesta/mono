def check(ctx: ProtoContext) -> list[Finding]:
    findings = []
    messages = {
        "message_removed": "protobuf message `{}` was removed",
        "enum_removed": "protobuf enum `{}` was removed",
        "field_removed": "protobuf field `{}` was removed",
        "field_number_changed": "protobuf field number changed for `{}`",
        "field_type_changed": "protobuf field type changed for `{}`",
        "field_label_changed": "protobuf field label changed for `{}`",
        "field_oneof_changed": "protobuf field oneof membership changed for `{}`",
        "enum_value_removed": "protobuf enum value `{}` was removed",
        "enum_value_number_changed": "protobuf enum value number changed for `{}`",
        "message_reserved_range_removed": "protobuf message `{}` no longer reserves a field-number range",
        "message_reserved_name_removed": "protobuf message `{}` no longer reserves a field name",
        "enum_reserved_range_removed": "protobuf enum `{}` no longer reserves a numeric range",
        "enum_reserved_name_removed": "protobuf enum `{}` no longer reserves a value name",
        "oneof_removed": "protobuf oneof `{}` was removed",
        "service_removed": "protobuf service `{}` was removed",
        "method_removed": "protobuf method `{}` was removed",
        "method_signature_changed": "protobuf method signature changed for `{}`",
        "package_changed": "protobuf package changed in `{}`",
        "syntax_changed": "protobuf syntax changed in `{}`",
        "map_entry_changed": "protobuf map-entry semantics changed for `{}`",
        "extension_removed": "protobuf extension `{}` was removed",
        "extension_number_changed": "protobuf extension field number changed for `{}`",
        "extension_type_changed": "protobuf extension type changed for `{}`",
        "extension_label_changed": "protobuf extension label changed for `{}`",
        "file_options_changed": "protobuf file options changed in `{}`",
        "message_options_changed": "protobuf message options changed for `{}`",
        "field_options_changed": "protobuf field options changed for `{}`",
        "oneof_options_changed": "protobuf oneof options changed for `{}`",
        "enum_options_changed": "protobuf enum options changed for `{}`",
        "enum_value_options_changed": "protobuf enum value options changed for `{}`",
        "service_options_changed": "protobuf service options changed for `{}`",
        "method_options_changed": "protobuf method options changed for `{}`",
        "extension_options_changed": "protobuf extension options changed for `{}`",
        "registered_option_removed": "registered protobuf option `{}` was removed",
        "registered_option_value_changed": "registered protobuf option value changed for `{}`",
    }

    for delta in ctx.deltas:
        message = messages.get(delta.kind.value)
        if message != None:
            findings.append(finding_for_delta(ctx, delta, message.format(delta.symbol)))
    return findings
