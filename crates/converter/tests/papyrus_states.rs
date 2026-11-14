//! Synthetic Skyrim PEX scripts executed against the shipped Luau runtime.

use converter::script::{Function, Instruction, ScriptConverter, TypedName, Value};
use mlua::{Lua, Table};

/// Writes the small subset of the Skyrim PEX format used by these fixtures.
struct Pex {
    strings: Vec<String>,
    body: Vec<u8>,
}

impl Pex {
    /// Starts the fixture class with a declared event marker and no properties.
    fn new(auto: &str, states: u16) -> Self {
        Self::class("StateFixture", "ParentFixture", auto, states)
    }

    /// Starts a class with explicit ancestry for inherited dispatch tests.
    fn class(name: &str, parent: &str, auto: &str, states: u16) -> Self {
        let mut pex = Self {
            strings: vec![],
            body: vec![],
        };
        pex.word(1); // object count
        pex.symbol(name);
        pex.body.extend(0u32.to_be_bytes()); // object size (filled below)
        pex.symbol(parent);
        pex.symbol(""); // documentation
        pex.body.extend(0u32.to_be_bytes()); // flags
        pex.symbol(auto);
        pex.word(1); // variables
        pex.symbol("EventValue");
        pex.symbol("int");
        pex.body.extend(0u32.to_be_bytes());
        pex.value(&Value::Integer(0));
        pex.word(0); // properties
        pex.word(states);
        pex
    }

    /// Replaces the initial empty property table with auto and explicit accessors.
    fn add_properties(&mut self) {
        let states = self.body.split_off(self.body.len() - 2);
        self.body.truncate(self.body.len() - 2);
        self.word(2);
        for (name, flags) in [("Counter", 7), ("Handled", 3)] {
            self.symbol(name);
            self.symbol("int");
            self.symbol("");
            self.body.extend(0u32.to_be_bytes());
            self.body.push(flags);
            if flags == 7 {
                self.symbol("eVeNtVaLuE");
            } else {
                let mut getter = returns("", 0);
                getter.instructions[0].args = vec![id("EVENTVALUE")];
                self.function(getter);
                let mut setter = returns("", 0);
                setter.return_type = "None".into();
                setter.params = vec![TypedName {
                    name: "Value".into(),
                    type_name: "int".into(),
                }];
                setter.instructions = vec![
                    Instruction {
                        opcode: 13,
                        args: vec![id("eventvalue"), id("vAlUe")],
                        varargs: vec![],
                    },
                    Instruction {
                        opcode: 26,
                        args: vec![Value::None],
                        varargs: vec![],
                    },
                ];
                self.function(setter);
            }
        }
        self.body.extend(states);
    }

    /// Appends a big-endian count or string index.
    fn word(&mut self, value: u16) {
        self.body.extend(value.to_be_bytes());
    }

    /// Interns a fixture string and writes its index.
    fn symbol(&mut self, value: &str) {
        let index = self
            .strings
            .iter()
            .position(|s| s == value)
            .unwrap_or_else(|| {
                self.strings.push(value.to_owned());
                self.strings.len() - 1
            });
        self.word(index.try_into().unwrap());
    }

    /// Encodes one instruction operand, independently of the production reader.
    fn value(&mut self, value: &Value) {
        match value {
            Value::None => self.body.push(0),
            Value::Identifier(s) | Value::String(s) => {
                self.body.push(if matches!(value, Value::Identifier(_)) {
                    1
                } else {
                    2
                });
                self.symbol(s);
            }
            Value::Integer(n) => {
                self.body.push(3);
                self.body.extend(n.to_be_bytes());
            }
            Value::Float(n) => {
                self.body.push(4);
                self.body.extend(n.to_be_bytes());
            }
            Value::Bool(b) => self.body.extend([5, u8::from(*b)]),
        }
    }

    /// Writes a named state, including empty states with no exported methods.
    fn state(&mut self, name: &str, functions: Vec<Function>) {
        self.symbol(name);
        self.word(functions.len().try_into().unwrap());
        for function in functions {
            self.symbol(&function.name);
            self.function(function);
        }
    }

    /// Encodes a PEX function body for both states and explicit property handlers.
    fn function(&mut self, function: Function) {
        self.symbol(&function.return_type);
        self.symbol("");
        self.body.extend(0u32.to_be_bytes());
        self.body.push(function.flags);
        for names in [&function.params, &function.locals] {
            self.word(names.len().try_into().unwrap());
            for name in names {
                self.symbol(&name.name);
                self.symbol(&name.type_name);
            }
        }
        self.word(function.instructions.len().try_into().unwrap());
        for instruction in function.instructions {
            self.body.push(instruction.opcode);
            for value in &instruction.args {
                self.value(value);
            }
            if matches!(instruction.opcode, 23..=25) {
                self.value(&Value::Integer(
                    instruction.varargs.len().try_into().unwrap(),
                ));
                for value in &instruction.varargs {
                    self.value(value);
                }
            }
        }
    }

    /// Produces a complete 3.2 Skyrim PEX with a literal header and string table.
    fn finish(mut self) -> Vec<u8> {
        let size = u32::try_from(self.body.len() - 8).unwrap();
        self.body[4..8].copy_from_slice(&size.to_be_bytes());
        let mut bytes = vec![0xfa, 0x57, 0xc0, 0xde, 3, 2, 0, 1];
        bytes.extend(0u64.to_be_bytes());
        for text in ["states.psc", "fixture", "fixture"] {
            bytes.extend(u16::try_from(text.len()).unwrap().to_be_bytes());
            bytes.extend(text.as_bytes());
        }
        bytes.extend(u16::try_from(self.strings.len()).unwrap().to_be_bytes());
        for text in self.strings {
            bytes.extend(u16::try_from(text.len()).unwrap().to_be_bytes());
            bytes.extend(text.as_bytes());
        }
        bytes.extend([0, 0, 0]); // no debug information or user flags
        bytes.extend(self.body);
        bytes
    }
}

/// Exercises parent fallback and lexical Parent calls through three emitted classes.
#[test]
fn pex_parent_dispatch_preserves_receiver_and_defining_class() {
    let lua = Lua::new();
    runtime(&lua);
    let mut grand = Pex::class("Grand", "", "", 1);
    grand.state("", vec![returns("Chain", 73), returns("OnlyGrand", 79)]);
    let mut parent_call = returns("Chain", 0);
    parent_call.locals = vec![TypedName {
        name: "result".into(),
        type_name: "int".into(),
    }];
    parent_call.instructions = vec![
        Instruction {
            opcode: 24,
            args: vec![Value::String("cHaIn".into()), id("result")],
            varargs: vec![],
        },
        Instruction {
            opcode: 26,
            args: vec![id("result")],
            varargs: vec![],
        },
    ];
    let mut parent = Pex::class("ParentFixture", "gRaNd", "", 2);
    let mut virtual_call = parent_call.clone();
    virtual_call.name = "Virtual".into();
    virtual_call.instructions[0] = Instruction {
        opcode: 23,
        args: vec![Value::String("Value".into()), id("self"), id("result")],
        varargs: vec![],
    };
    parent.state(
        "",
        vec![
            parent_call.clone(),
            virtual_call,
            returns("OnlyParent", 67),
            returns("Value", 41),
            returns("InheritedState", 59),
            returns("Fallback", 57),
        ],
    );
    parent.state(
        "Ready",
        vec![returns("InheritedState", 61), returns("Fallback", 63)],
    );
    let mut child = Pex::class("StateFixture", "pArEnTfIxTuRe", "Ready", 2);
    child.state("", vec![parent_call, returns("Fallback", 17)]);
    child.state("Ready", vec![returns("Value", 31)]);
    for (name, pex) in [
        ("Grand", grand),
        ("ParentFixture", parent),
        ("Child", child),
    ] {
        lua.globals().set(name, load(&lua, &pex.finish())).unwrap();
    }
    lua.load(
        r#"
        local rt = Runtime.new()
        rt:register_class('Grand', Grand)
        rt:register_class('ParentFixture', ParentFixture)
        rt:register_class('StateFixture', Child)
        local a = Child.new(rt)
        local function call(name) return rt:call_method(a, name, {}) end
        assert(call('iNhErItEdStAtE') == 61, 'parent named state')
        assert(call('OnlyParent') == 67 and call('OnlyGrand') == 79, 'ancestor fallback')
        assert(call('Fallback') == 17, 'child empty state precedes parent per dispatch contract')
        assert(call('Chain') == 73, 'nested Parent calls use the defining class')
        assert(call('vIrTuAl') == 31, 'inherited code keeps the original receiver')
    "#,
    )
    .exec()
    .unwrap();
}

/// Shows lifecycle errors stay within one invocation, with predictable state visibility.
#[test]
fn lifecycle_failures_leave_other_instances_callable() {
    let lua = Lua::new();
    runtime(&lua);
    lua.load(r#"
        local rt = Runtime.new()
        local script = { __name = 'Broken', __parent = '', __autoState = 'Old',
            __states = { Old = true, New = true },
            Value = function(self) return 91 end,
            ['Old::OnEndState'] = function(self, ...) assert(select('#', ...) == 0); error('end failed') end,
            ['New::OnBeginState'] = function(self, ...) assert(select('#', ...) == 0); error('begin failed') end }
        local a, b = setmetatable({}, {__index = script}), setmetatable({}, {__index = script})
        local ok, err = pcall(rt.goto_state, rt, a, 'New')
        assert(not ok and string.find(err, 'end failed'))
        assert(rt:call_method(a, 'GetState', {}) == 'Old')
        script['Old::OnEndState'] = nil
        ok, err = pcall(rt.goto_state, rt, a, 'New')
        assert(not ok and string.find(err, 'begin failed'))
        assert(rt:call_method(a, 'GetState', {}) == 'New')
        assert(rt:call_method(b, 'Value', {}) == 91)
        script['New::OnBeginState'] = nil
        rt:goto_state(b, 'New')
        assert(rt:call_method(b, 'GetState', {}) == 'New', 'absent lifecycle handlers are optional')
        rt:register_class('Broken', script)
        script.__parent = 'Broken'
        ok, err = pcall(rt.call_method, rt, a, 'Missing', {})
        assert(not ok and string.find(err, 'cyclic Papyrus parent hierarchy'))
        assert(rt:call_method(b, 'Value', {}) == 91)
    "#).exec().unwrap();
}

/// An undeclared initial state uses empty-state fallback and ordinary errors stay local.
#[test]
fn undeclared_auto_state_uses_empty_fallback() {
    let lua = Lua::new();
    runtime(&lua);
    let mut unlisted = Pex::new("Typo", 1);
    unlisted.state("", vec![returns("Value", 11)]);
    let mut good = Pex::new("", 1);
    good.state("", vec![returns("Value", 93)]);
    let unlisted = load(&lua, &unlisted.finish());
    let good = load(&lua, &good.finish());
    let runtime: Table = lua.load("return Runtime.new()").eval().unwrap();
    let initial: Table = unlisted
        .get::<mlua::Function>("new")
        .unwrap()
        .call(runtime.clone())
        .expect("undeclared initial state can use empty fallback");
    let dispatch = runtime.get::<mlua::Function>("call_method").unwrap();
    let value: i32 = dispatch
        .call((
            runtime.clone(),
            initial.clone(),
            "Value",
            lua.create_table().unwrap(),
        ))
        .unwrap();
    assert_eq!(value, 11);
    let state: String = dispatch
        .call((
            runtime.clone(),
            initial,
            "GetState",
            lua.create_table().unwrap(),
        ))
        .unwrap();
    assert_eq!(state, "Typo");
    let instance: Table = good
        .get::<mlua::Function>("new")
        .unwrap()
        .call(runtime.clone())
        .unwrap();
    let dispatch = runtime.get::<mlua::Function>("call_method").unwrap();
    let failing: Table = good
        .get::<mlua::Function>("new")
        .unwrap()
        .call(runtime.clone())
        .unwrap();
    let error = dispatch
        .call::<()>((
            runtime.clone(),
            failing.clone(),
            "Absent",
            lua.create_table().unwrap(),
        ))
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("unknown Papyrus method: StateFixture::Absent")
    );
    let value: i32 = dispatch
        .call((runtime, instance, "Value", lua.create_table().unwrap()))
        .unwrap();
    assert_eq!(
        value, 93,
        "another script still runs after Rust receives an invocation error"
    );
}

/// Runs a complete synthetic conversion while draining its progress channel.
async fn convert(config: converter::PipelineConfig) -> converter::PipelineReport {
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let report = converter::AssetPipeline::run_async(config, tx)
        .await
        .unwrap();
    drain.await.unwrap();
    assert!(report.complete, "{:?}", report.warnings);
    report
}

/// ABI-1 cached scripts rebuild without invalidating mesh or texture output proofs.
#[tokio::test]
async fn old_script_abi_rebuilds_only_scripts_even_with_legacy_configuration() {
    rebuild_old_script_abi(1).await;
}

/// The immediately preceding emitter ABI also rebuilds with saved legacy settings.
#[tokio::test]
async fn abi_two_scripts_regenerate_case_insensitive_helpers() {
    rebuild_old_script_abi(2).await;
}

/// Installs actual prior emitted bytes and verifies script-only cache invalidation.
async fn rebuild_old_script_abi(abi: u32) {
    use converter::cache::{
        ConversionManifest, asset_configuration_hash_for_schema, configuration_hash, hash_bytes,
    };
    use dummy_content::layout;
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("Data");
    let output = directory.path().join("modern");
    layout::prepare_directory(&data, false).unwrap();
    layout::generate(
        &data,
        layout::DEFAULT_SEED,
        layout::Formats::parse("dds,nif,pex,esm").unwrap(),
    )
    .unwrap();
    let config = converter::PipelineConfig::new(&data, &output);
    convert(config.clone()).await;
    let manifest_path = output.join("conversion-manifest.json");
    let mut manifest: ConversionManifest =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    let mut legacy = config.clone();
    legacy.script_abi_version = abi;
    manifest.configuration_hash = configuration_hash(&legacy).unwrap();
    manifest.asset_configuration_hashes.insert(
        "pex".into(),
        asset_configuration_hash_for_schema(&legacy, manifest.schema_version, "pex").unwrap(),
    );
    // An actual ABI-1 minimal script's output: no state registry or instance state.
    let old = concat!(
        "-- Generated from test.psc by Mudcrab\n",
        "-- Papyrus bytecode 3.2; deterministic PC-state lowering\n",
        "local Script = {}\nScript.__index = Script\nScript.__name = \"Generated\"\n",
        "Script.__parent = \"ObjectReference\"\nScript.__autoState = \"\"\nScript.__types = {\n}\n",
        "function Script.new(runtime)\n    assert(runtime, \"Papyrus runtime is required\")\n",
        "    local self = setmetatable({ __runtime = runtime }, Script)\n    return self\nend\n",
        "Script[\"Run\"] = function(self)\n    local __rt = self.__runtime\n",
        "    local __v, __locals, __types = {}, {}, {}\n",
        "    local function __get(key) if key == \"self\" then return self elseif __locals[key] then return __v[key] else return self[key] end end\n",
        "    local function __set(key, value) if __locals[key] then __v[key] = value else self[key] = value end end\n",
        "    local __pc = 0\n    while true do\n        if __pc == 1 then return nil end\n",
        "        if __pc < 0 or __pc >= 1 then error(\"invalid Papyrus program counter: \" .. tostring(__pc)) end\n",
        "        if __pc >= 0 and __pc <= 0 then\n            if __pc == 0 then\n",
        "            return nil\n            end\n            continue\n",
        "        end\n    end\nend\nreturn Script\n"
    );
    let old = if abi == 2 {
        old.replace("Script.__autoState = \"\"\n", "Script.__autoState = \"\"\nScript.__states = { [\"\"] = true,\n    [\"\"] = true,\n}\n")
            .replace("{ __runtime = runtime }", "{ __runtime = runtime, __state = Script.__autoState }")
            .replace("    return self\n", "    assert(Script.__states[Script.__autoState], \"unknown Papyrus auto state: \" .. Script.__name .. \"::\" .. Script.__autoState)\n    return self\n")
    } else {
        old.to_owned()
    };
    // Fixed SHA-256 proofs pin the historical output independently of current emission.
    use sha2::{Digest, Sha256};
    let expected = if abi == 1 {
        "5f64b25597b527984312044b2cd39179a4754669340fb18e8c551e785edc8be4"
    } else {
        "98a3a1424dbd8d510f479f49fb44dace43bd8ca6fa056dce3867de27b463194f"
    };
    assert_eq!(format!("{:x}", Sha256::digest(old.as_bytes())), expected);
    let mut scripts = 0;
    for entry in manifest
        .entries
        .values_mut()
        .filter(|entry| entry.output.ends_with(".luau"))
    {
        std::fs::write(output.join(&entry.output), &old).unwrap();
        entry.output_hash = hash_bytes(old.as_bytes());
        scripts += 1;
    }
    assert!(scripts > 0);
    manifest.save(&manifest_path).unwrap();
    // Old saved configuration must not opt the current emitter back into prior-ABI reuse.
    let report = convert(legacy).await;
    assert_eq!(report.converted, scripts);
    assert_eq!(report.cache_hits, manifest.entries.len() as u64 - scripts);
    for entry in manifest
        .entries
        .values()
        .filter(|entry| entry.output.ends_with(".luau"))
    {
        let source = std::fs::read_to_string(output.join(&entry.output)).unwrap();
        assert!(
            source.contains("__rt:identifier_key(__locals, key)"),
            "legacy script must be regenerated"
        );
    }
    assert_eq!(
        std::fs::read_to_string(output.join("scripts/papyrus_runtime.luau")).unwrap(),
        include_str!("../../shared/src/papyrus_runtime.luau")
    );
}

/// Builds an operand naming a local or instance field.
fn id(name: &str) -> Value {
    Value::Identifier(name.into())
}

/// Builds a tiny return function or event whose return value identifies the state.
fn returns(name: &str, value: i32) -> Function {
    Function {
        name: name.into(),
        return_type: "int".into(),
        flags: 0,
        params: vec![],
        locals: vec![],
        instructions: vec![Instruction {
            opcode: 26,
            args: vec![Value::Integer(value)],
            varargs: vec![],
        }],
    }
}

/// Builds a void Papyrus event that updates a declared instance variable.
fn event(value: i32) -> Function {
    Function {
        return_type: "None".into(),
        instructions: vec![
            Instruction {
                opcode: 13,
                args: vec![id("EventValue"), Value::Integer(value)],
                varargs: vec![],
            },
            Instruction {
                opcode: 26,
                args: vec![Value::None],
                varargs: vec![],
            },
        ],
        ..returns("OnPulse", 0)
    }
}

/// Changes state using emitted CALLMETHOD instructions and returns GetState.
fn switch() -> Function {
    Function {
        params: vec![TypedName {
            name: "destination".into(),
            type_name: "string".into(),
        }],
        locals: vec![
            TypedName {
                name: "result".into(),
                type_name: "string".into(),
            },
            TypedName {
                name: "unused".into(),
                type_name: "None".into(),
            },
        ],
        return_type: "string".into(),
        instructions: vec![
            Instruction {
                opcode: 23,
                args: vec![Value::String("gOtOsTaTe".into()), id("self"), id("unused")],
                varargs: vec![id("destination")],
            },
            Instruction {
                opcode: 23,
                args: vec![Value::String("gEtStAtE".into()), id("self"), id("result")],
                varargs: vec![],
            },
            Instruction {
                opcode: 26,
                args: vec![id("result")],
                varargs: vec![],
            },
        ],
        ..returns("Switch", 0)
    }
}

/// Logs a transition event via a native, including GetState and argument count.
fn transition(name: &str, label: &str) -> Function {
    let mut function = returns(name, 0);
    function.return_type = "None".into();
    function.locals = vec![
        TypedName {
            name: "current".into(),
            type_name: "string".into(),
        },
        TypedName {
            name: "unused".into(),
            type_name: "None".into(),
        },
    ];
    function.instructions = vec![
        Instruction {
            opcode: 23,
            args: vec![Value::String("gEtStAtE".into()), id("self"), id("current")],
            varargs: vec![],
        },
        Instruction {
            opcode: 23,
            args: vec![Value::String("Record".into()), id("self"), id("unused")],
            varargs: vec![Value::String(label.into()), id("current")],
        },
        Instruction {
            opcode: 26,
            args: vec![Value::None],
            varargs: vec![],
        },
    ];
    function
}

/// Compiles fixture bytes through the file converter and loads the resulting class.
fn load(lua: &Lua, bytes: &[u8]) -> Table {
    let temp = tempfile::tempdir().unwrap();
    let input = temp.path().join("fixture.pex");
    let output = temp.path().join("fixture.luau");
    std::fs::write(&input, bytes).unwrap();
    ScriptConverter::convert_pex_to_luau(&input, &output).unwrap();
    lua.load(std::fs::read_to_string(output).unwrap())
        .eval()
        .unwrap()
}

/// Installs the exact helper shipped in converted packs and used by the Rust host.
fn runtime(lua: &Lua) {
    let module: Table = lua
        .load(include_str!("../../shared/src/papyrus_runtime.luau"))
        .eval()
        .unwrap();
    lua.globals().set("Runtime", module).unwrap();
}

/// Distinguishes auto, named and empty dispatch, lifecycle order, fallback and instance isolation.
#[test]
fn pex_state_dispatch_and_transitions() {
    let mut pex = Pex::new("rEaDy", 4);
    pex.state(
        "",
        vec![
            returns("Value", 11),
            event(12),
            returns("Fallback", 17),
            switch(),
            transition("oNbEgInStAtE", "empty-begin"),
            transition("oNeNdStAtE", "empty-end"),
            Function {
                name: "Record".into(),
                return_type: "None".into(),
                flags: 2,
                instructions: vec![],
                params: vec![
                    TypedName {
                        name: "label".into(),
                        type_name: "string".into(),
                    },
                    TypedName {
                        name: "state".into(),
                        type_name: "string".into(),
                    },
                ],
                ..returns("Record", 0)
            },
        ],
    );
    pex.state(
        "Ready",
        vec![
            returns("Value", 31),
            event(32),
            transition("oNeNdStAtE", "ready-end"),
        ],
    );
    pex.state(
        "Busy",
        vec![
            returns("Value", 51),
            event(52),
            transition("oNbEgInStAtE", "busy-begin"),
            transition("oNeNdStAtE", "busy-end"),
        ],
    );
    pex.state("Quiet", vec![]);
    let bytes = pex.finish();
    let lua = Lua::new();
    runtime(&lua);
    let class = load(&lua, &bytes);
    lua.globals().set("Script", class).unwrap();
    lua.load(r#"
        local rt = Runtime.new()
        rt:register_class("StateFixture", Script)
        local log = {}
        rt:register_native("StateFixture", "Record", function(self, ...)
            assert(select('#', ...) == 2, 'native log receives label and current state')
            local label, state = ...
            table.insert(log, label .. ':' .. state)
        end)
        local a, b = Script.new(rt), Script.new(rt)
        local function call(target, name, ...) return rt:call_method(target, name, {...}) end
        assert(Script.__states.ready and Script.__states.busy)
        assert(call(a, 'GetState') == 'rEaDy')
        assert(call(a, 'vAlUe') == 31, 'auto method dispatch')
        call(a, 'oNpUlSe')
        assert(a.EventValue == 32, 'auto event dispatch')
        assert(call(a, 'Fallback') == 17)
        assert(#log == 0, 'auto state starts without transition events')
        assert(call(a, 'sWiTcH', 'bUsY') == 'bUsY')
        assert(call(a, 'GetState') == 'bUsY')
        call(a, 'oNpUlSe')
        assert(call(a, 'vAlUe') == 51 and a.EventValue == 52)
        assert(call(a, 'Fallback') == 17)
        assert(table.concat(log, ',') == 'ready-end:rEaDy,busy-begin:bUsY')
        call(a, 'gOtOsTaTe', 'bUsY')
        assert(table.concat(log, ',') == 'ready-end:rEaDy,busy-begin:bUsY,busy-end:bUsY,busy-begin:bUsY')
        call(a, 'GotoState', '')
        assert(call(a, 'GetState') == '')
        call(a, 'oNpUlSe')
        assert(call(a, 'vAlUe') == 11 and a.EventValue == 12)
        assert(log[5] == 'busy-end:bUsY' and log[6] == 'empty-begin:')
        call(a, 'GotoState', 'Quiet')
        assert(call(a, 'GetState') == 'Quiet' and call(a, 'vAlUe') == 11)
        assert(log[7] == 'empty-end:' and log[8] == 'empty-begin:Quiet')
        assert(call(b, 'GetState') == 'rEaDy' and call(b, 'Value') == 31)
        local before_unknown = #log
        assert(call(a, 'Switch', 'Not Declared!') == 'Not Declared!')
        assert(call(a, 'GetState') == 'Not Declared!', 'unknown name stored verbatim')
        call(a, 'oNpUlSe')
        assert(call(a, 'vAlUe') == 11 and a.EventValue == 12, 'unknown state uses empty functions and events')
        assert(log[before_unknown+1] == 'empty-end:Quiet' and log[before_unknown+2] == 'empty-begin:Not Declared!')
        call(a, 'GotoState', 'Ready')
        assert(log[before_unknown+3] == 'empty-end:Not Declared!' and log[before_unknown+4] == 'empty-begin:Ready')
        local ok, err = pcall(call, a, 'Absent')
        assert(not ok and string.find(err, 'unknown Papyrus method'))
        assert(call(b, 'Value') == 31, 'another script still runs after errors')
    "#).exec().unwrap();
}

/// Undeclared GotoState stores the argument and uses empty lifecycle and method fallback.
#[test]
fn undeclared_destination_falls_back_and_notifies() {
    let lua = Lua::new();
    runtime(&lua);
    let mut pex = Pex::new("Ready", 2);
    pex.state(
        "",
        vec![
            returns("Value", 11),
            switch(),
            transition("oNbEgInStAtE", "begin"),
            transition("oNeNdStAtE", "end"),
            Function {
                flags: 2,
                return_type: "None".into(),
                instructions: vec![],
                params: vec![
                    TypedName {
                        name: "label".into(),
                        type_name: "string".into(),
                    },
                    TypedName {
                        name: "state".into(),
                        type_name: "string".into(),
                    },
                ],
                ..returns("Record", 0)
            },
        ],
    );
    pex.state("Ready", vec![returns("Value", 31)]);
    lua.globals()
        .set("Script", load(&lua, &pex.finish()))
        .unwrap();
    lua.load(r#"
        local rt, log = Runtime.new(), {}
        rt:register_native('StateFixture', 'Record', function(self, label, state) table.insert(log, label .. ':' .. state) end)
        local a, b = Script.new(rt), Script.new(rt)
        assert(rt:call_method(a, 'Switch', {'Not Declared!'}) == 'Not Declared!')
        assert(rt:call_method(a, 'GetState', {}) == 'Not Declared!')
        assert(rt:call_method(a, 'Value', {}) == 11)
        assert(table.concat(log, ',') == 'end:Ready,begin:Not Declared!')
        rt:goto_state(a, 'Ready')
        assert(table.concat(log, ',') == 'end:Ready,begin:Not Declared!,end:Not Declared!,begin:Ready')
        assert(rt:call_method(b, 'Value', {}) == 31)
    "#).exec().unwrap();
}

/// Mixed-case local/field operands, static calls, class identifiers and natives share name resolution.
#[test]
fn pex_mixed_case_identifiers_static_calls_and_natives() {
    let lua = Lua::new();
    runtime(&lua);
    let mut pex = Pex::class("CaseFixture", "", "", 1);
    let mut run = returns("Run", 0);
    run.locals = vec![TypedName {
        name: "Result".into(),
        type_name: "int".into(),
    }];
    run.instructions = vec![
        Instruction {
            opcode: 13,
            args: vec![id("eVeNtVaLuE"), Value::Integer(37)],
            varargs: vec![],
        },
        Instruction {
            opcode: 25,
            args: vec![
                Value::String("cAsEfIxTuRe".into()),
                Value::String("cOmPuTe".into()),
                id("rEsUlT"),
            ],
            varargs: vec![id("EVENTVALUE")],
        },
        Instruction {
            opcode: 14,
            args: vec![id("RESULT"), id("rEsUlT")],
            varargs: vec![],
        },
        Instruction {
            opcode: 26,
            args: vec![id("RESULT")],
            varargs: vec![],
        },
    ];
    let mut compute = returns("Compute", 0);
    compute.return_type = "float".into();
    compute.flags = 1; // global bytecode, distinct from the global native below
    compute.params = vec![TypedName {
        name: "Input".into(),
        type_name: "int".into(),
    }];
    compute.locals = vec![TypedName {
        name: "Answer".into(),
        type_name: "float".into(),
    }];
    compute.instructions = vec![
        Instruction {
            opcode: 25,
            args: vec![
                Value::String("CASEFIXTURE".into()),
                Value::String("dOuBlE".into()),
                id("ANSWER"),
            ],
            varargs: vec![id("iNpUt")],
        },
        Instruction {
            opcode: 26,
            args: vec![id("answer")],
            varargs: vec![],
        },
    ];
    pex.state(
        "",
        vec![
            run,
            compute,
            Function {
                flags: 3,
                return_type: "float".into(),
                instructions: vec![],
                params: vec![TypedName {
                    name: "Value".into(),
                    type_name: "int".into(),
                }],
                ..returns("Double", 0)
            },
        ],
    );
    lua.globals()
        .set("Script", load(&lua, &pex.finish()))
        .unwrap();
    lua.load(r#"
        local rt = Runtime.new()
        rt:register_class('CASEFIXTURE', Script)
        rt:register_native('casefixture', 'DOUBLE', function(self, value) return value * 2 + 0.75 end)
        assert(rt:resolve_identifier(nil, 'cAsEfIxTuRe') == Script)
        local a = Script.new(rt)
        assert(rt:call_method(a, 'Run', {}) == 74)
        assert(a.EventValue == 37 and a.eVeNtVaLuE == nil, 'one canonical field')
        local other = Runtime.new()
        other:register_class('casefixture', Script)
        other:register_native('CASEFIXTURE', 'double', function(self, value) return value * 3 + 0.75 end)
        assert(other:call_method(Script.new(other), 'RUN', {}) == 111)
        assert(rt:call_method(a, 'Run', {}) == 74, 'static contexts use the invoking runtime')
        assert(Script.__runtime == nil, 'class registration data stays immutable')
    "#).exec().unwrap();
}

/// Auto and explicit properties resolve mixed-case accessors and one backing field through PEX opcodes.
#[test]
fn pex_mixed_case_properties_use_canonical_backing_fields() {
    let lua = Lua::new();
    runtime(&lua);
    let mut pex = Pex::new("", 1);
    pex.add_properties();
    let mut run = returns("Run", 0);
    run.locals = vec![TypedName {
        name: "Result".into(),
        type_name: "int".into(),
    }];
    run.instructions = vec![
        Instruction {
            opcode: 29,
            args: vec![
                Value::String("cOuNtEr".into()),
                id("SELF"),
                Value::Integer(43),
            ],
            varargs: vec![],
        },
        Instruction {
            opcode: 28,
            args: vec![Value::String("COUNTER".into()), id("self"), id("RESULT")],
            varargs: vec![],
        },
        Instruction {
            opcode: 26,
            args: vec![id("Result")],
            varargs: vec![],
        },
    ];
    pex.state("", vec![run]);
    lua.globals()
        .set("Script", load(&lua, &pex.finish()))
        .unwrap();
    lua.load(r#"
        local rt = Runtime.new()
        local a = Script.new(rt)
        assert(rt:call_method(a, 'Run', {}) == 43)
        assert(a.EventValue == 43 and a.eVeNtVaLuE == nil)
        rt:set_property(a, 'hAnDlEd', 67)
        assert(rt:get_property(a, 'HANDLED') == 67)
        rt:set_property(a, 'EVENTVALUE', 89)
        assert(rt:get_property(a, 'eventvalue') == 89 and a.EventValue == 89)
        a.EventValue = nil
        rt:set_property(a, 'eVeNtVaLuE', 97)
        assert(a.EventValue == 97 and a.eVeNtVaLuE == nil, 'None keeps its declared field name')
        rt:register_class('PARENT', { get_Inherited = function(self) return self.EventValue + 2 end })
        Script.__parent = 'pArEnT'
        assert(rt:get_property(a, 'iNhErItEd') == 99)
    "#).exec().unwrap();
}
