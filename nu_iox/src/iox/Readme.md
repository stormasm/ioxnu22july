
```rust
// This compiles and returns nothing
Ok(PipelineData::Value(
    Value::Nothing { span: call.head },
    None,
))
```

### nu_protocol

- shell_error.rs

Possible errors I can use for iox

- IncompatibleParametersSingle
- UnsupportedInput
- FileNotFound
- IOError
- ReadingFile
- GenericError