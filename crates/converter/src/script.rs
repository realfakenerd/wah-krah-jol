use color_eyre::{
    Result,
    eyre::{WrapErr, bail, ensure, eyre},
};
use memmap2::Mmap;
use std::{
    cell::RefCell,
    collections::BTreeSet,
    fmt::Write as _,
    fs::{self, File},
    path::Path,
};

thread_local! {
    // One Lua VM per worker thread for syntax and chunk validation
    static THREAD_LUA: RefCell<mlua::Lua> = RefCell::new(mlua::Lua::new());
    // Reusable string buffer
    static THREAD_STRING_BUF: RefCell<String> = RefCell::new(String::with_capacity(32 * 1024));
}

/// Script emission ABI shared with runtime contract definitions.
pub const SCRIPT_ABI_VERSION: u32 = shared::PAPYRUS_SCRIPT_ABI_VERSION;

const SKYRIM_MAGIC: u32 = 0xFA57_C0DE;
const OPCODE_ARGS: [usize; 36] = [
    0, 3, 3, 3, 3, 3, 3, 3, 3, 3, 2, 2, 2, 2, 2, 3, 3, 3, 3, 3, 1, 2, 2, 3, 2, 3, 1, 3, 3, 3, 2, 2,
    3, 3, 4, 4,
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PexHeader {
    pub major_version: u8,
    pub minor_version: u8,
    pub game_id: u16,
    pub compilation_time: u64,
    pub source_file: String,
    pub user_name: String,
    pub machine_name: String,
    pub strings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    None,
    Identifier(String),
    String(String),
    Integer(i32),
    Float(f32),
    Bool(bool),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypedName {
    pub name: String,
    pub type_name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Instruction {
    pub opcode: u8,
    pub args: Vec<Value>,
    pub varargs: Vec<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Function {
    pub name: String,
    pub return_type: String,
    pub flags: u8,
    pub params: Vec<TypedName>,
    pub locals: Vec<TypedName>,
    pub instructions: Vec<Instruction>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Variable {
    pub name: String,
    pub type_name: String,
    pub default: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Property {
    pub name: String,
    pub type_name: String,
    pub auto_var: Option<String>,
    pub readable: bool,
    pub writable: bool,
    pub read: Option<Function>,
    pub write: Option<Function>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct State {
    pub name: String,
    pub functions: Vec<Function>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Object {
    pub name: String,
    pub parent: String,
    pub auto_state: String,
    pub variables: Vec<Variable>,
    pub properties: Vec<Property>,
    pub states: Vec<State>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PexFile {
    pub header: PexHeader,
    pub objects: Vec<Object>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BasicBlock {
    pub start: usize,
    pub end: usize,
    pub successors: Vec<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlFlowGraph {
    pub blocks: Vec<BasicBlock>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IrInstruction {
    pub pc: usize,
    pub instruction: Instruction,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FunctionIr {
    pub cfg: ControlFlowGraph,
    pub instructions: Vec<IrInstruction>,
}

pub struct ScriptConverter;

impl ScriptConverter {
    pub fn convert_pex_to_luau(input: &Path, output: &Path) -> Result<()> {
        let bytes =
            File::open(input).wrap_err_with(|| format!("failed to open {}", input.display()))?;

        let mmap = unsafe { Mmap::map(&bytes) }
            .wrap_err_with(|| format!("failed to memory-map {}", input.display()))?;

        let pex = Self::parse(&mmap)?;
        Self::verify(&pex)?;

        // Use thread-local reusable scratch string buffer
        THREAD_STRING_BUF.with(|buf_cell| -> Result<()> {
            let mut buf = buf_cell.borrow_mut();
            buf.clear();
            Self::emit_luau_to(&pex, &mut buf)?;

            THREAD_LUA.with(|lua_cell| {
                let lua = lua_cell.borrow();
                lua.load(&*buf)
                    .set_name(input.to_string_lossy())
                    .into_function()
                    .map_err(|err| eyre!("generated invalid Luau: {err}"))?;
                Ok::<(), color_eyre::Report>(())
            })?;

            if let Some(parent) = output.parent() {
                fs::create_dir_all(parent)?;
            }
            // Staged outputs may share an inode with a previous pack via
            // hard link; replace the path instead of writing through it.
            // Through a temporary file and a rename, so a run killed mid-write never leaves a torn
            // script under its final name for a resumed run to accept. The temporary name is
            // unique per call and created exclusively, so two calls for one output can't share it.
            static NEXT_TEMPORARY: std::sync::atomic::AtomicU64 =
                std::sync::atomic::AtomicU64::new(0);
            let temporary = output.with_file_name(format!(
                ".{}.{}-{}.partial",
                output.file_name().unwrap_or_default().to_string_lossy(),
                std::process::id(),
                NEXT_TEMPORARY.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .and_then(|mut file| {
                    use std::io::Write as _;
                    file.write_all(buf.as_bytes())
                })
                .and_then(|()| {
                    if output.is_file() {
                        let _ = fs::remove_file(output);
                    }
                    fs::rename(&temporary, output)
                })
                .inspect_err(|_| {
                    let _ = fs::remove_file(&temporary);
                })
                .wrap_err_with(|| format!("failed to write {}", output.display()))?;
            Ok(())
        })
    }

    pub fn parse_header(bytes: &[u8]) -> Result<PexHeader> {
        let mut reader = Reader::new(bytes);
        let mut header = parse_header_prefix(&mut reader)?;
        header.strings = reader.string_table()?;
        Ok(header)
    }

    pub fn parse(bytes: &[u8]) -> Result<PexFile> {
        let mut reader = Reader::new(bytes);
        let mut header = parse_header_prefix(&mut reader)?;
        header.strings = reader.string_table()?;
        let strings = header.strings.clone();
        skip_debug_info(&mut reader, &strings)?;
        let user_flags = reader.u16()? as usize;
        for _ in 0..user_flags {
            reader.string_ref(&strings)?;
            reader.u8()?;
        }
        let object_count = reader.u16()? as usize;
        let mut objects = Vec::with_capacity(object_count);
        for _ in 0..object_count {
            objects.push(parse_object(&mut reader, &strings)?);
        }
        ensure!(
            reader.remaining() == 0,
            "{} trailing bytes after PEX object table",
            reader.remaining()
        );
        Ok(PexFile { header, objects })
    }

    pub fn verify(pex: &PexFile) -> Result<()> {
        ensure!(
            pex.header.game_id == 1,
            "only Skyrim PEX game id 1 is supported"
        );
        ensure!(!pex.objects.is_empty(), "PEX contains no script objects");
        ensure!(
            pex.objects.len() == 1,
            "multi-object PEX files are not supported"
        );
        for object in &pex.objects {
            ensure!(!object.name.is_empty(), "PEX object has an empty name");
            for state in &object.states {
                for function in &state.functions {
                    build_cfg(function).wrap_err_with(|| {
                        format!("invalid function {}.{}", object.name, function.name)
                    })?;
                }
            }
            for property in &object.properties {
                for function in property.read.iter().chain(property.write.iter()) {
                    build_cfg(function).wrap_err_with(|| {
                        format!("invalid property handler {}.{}", object.name, property.name)
                    })?;
                }
            }
        }
        Ok(())
    }

    pub fn emit_luau(pex: &PexFile) -> Result<String> {
        let mut out = String::with_capacity(16 * 1024);
        Self::emit_luau_to(pex, &mut out)?;
        Ok(out)
    }

    /// Emits verified class metadata, instance initialization and functions into a reusable buffer.
    pub fn emit_luau_to(pex: &PexFile, out: &mut String) -> Result<()> {
        let object = pex.objects.first().expect("verified object");

        writeln!(
            out,
            "-- Generated from {} by Mudcrab",
            pex.header.source_file
        )?;
        writeln!(
            out,
            "-- Papyrus bytecode {}.{}; deterministic PC-state lowering",
            pex.header.major_version, pex.header.minor_version
        )?;
        writeln!(out, "local Script = {{}}")?;
        writeln!(out, "Script.__index = Script")?;
        writeln!(out, "Script.__name = {}", lua_string(&object.name))?;
        writeln!(out, "Script.__parent = {}", lua_string(&object.parent))?;
        writeln!(
            out,
            "Script.__autoState = {}",
            lua_string(&object.auto_state)
        )?;
        writeln!(out, "Script.__states = {{ [\"\"] = true,")?;
        for state in &object.states {
            writeln!(
                out,
                "    [{}] = true,",
                lua_string(&state.name.to_ascii_lowercase())
            )?;
        }
        writeln!(out, "}}")?;
        writeln!(out, "Script.__types = {{")?;
        for variable in &object.variables {
            writeln!(
                out,
                "    [{}] = {},",
                lua_string(&variable.name),
                lua_string(&variable.type_name)
            )?;
        }
        writeln!(out, "}}")?;
        writeln!(out, "function Script.new(runtime)")?;
        writeln!(out, "    assert(runtime, \"Papyrus runtime is required\")")?;
        writeln!(
            out,
            "    local self = setmetatable({{ __runtime = runtime, __state = Script.__autoState }}, Script)"
        )?;
        for variable in &object.variables {
            writeln!(
                out,
                "    self[{}] = {}",
                lua_string(&variable.name),
                default_value_expr(&variable.default)
            )?;
        }
        writeln!(out, "    return self")?;
        writeln!(out, "end")?;
        for state in &object.states {
            for function in &state.functions {
                emit_function(out, state, function)?;
            }
        }
        for property in &object.properties {
            if let Some(auto_var) = &property.auto_var {
                if property.readable {
                    writeln!(
                        out,
                        "Script[{}] = function(self) return self.__runtime:get_field(self, {}) end",
                        lua_string(&format!("get_{}", property.name)),
                        lua_string(auto_var)
                    )?;
                }
                if property.writable {
                    writeln!(
                        out,
                        "Script[{}] = function(self, value) self.__runtime:set_field(self, {}, value) end",
                        lua_string(&format!("set_{}", property.name)),
                        lua_string(auto_var)
                    )?;
                }
            }
            if let Some(function) = &property.read {
                let mut named = function.clone();
                named.name = format!("get_{}", property.name);
                emit_function(
                    out,
                    &State {
                        name: String::new(),
                        functions: vec![],
                    },
                    &named,
                )?;
            }
            if let Some(function) = &property.write {
                let mut named = function.clone();
                named.name = format!("set_{}", property.name);
                emit_function(
                    out,
                    &State {
                        name: String::new(),
                        functions: vec![],
                    },
                    &named,
                )?;
            }
        }
        writeln!(out, "return Script")?;
        Ok(())
    }
}

pub fn build_cfg(function: &Function) -> Result<ControlFlowGraph> {
    if function.flags & 0x02 != 0 {
        ensure!(
            function.instructions.is_empty(),
            "native function contains bytecode"
        );
        return Ok(ControlFlowGraph { blocks: vec![] });
    }
    if function.instructions.is_empty() {
        return Ok(ControlFlowGraph { blocks: vec![] });
    }
    let len = function.instructions.len();
    let mut leaders = BTreeSet::from([0usize]);
    for (ip, instruction) in function.instructions.iter().enumerate() {
        if matches!(instruction.opcode, 20..=22) {
            let offset_arg = if instruction.opcode == 20 { 0 } else { 1 };
            let Value::Integer(offset) = instruction.args[offset_arg] else {
                bail!("jump at {ip} has a non-integer offset")
            };
            let target = ip as isize + offset as isize;
            ensure!(
                (0..=len as isize).contains(&target),
                "jump at {ip} targets {target}"
            );
            if target < len as isize {
                leaders.insert(target as usize);
            }
            if ip + 1 < len {
                leaders.insert(ip + 1);
            }
        } else if instruction.opcode == 26 && ip + 1 < len {
            leaders.insert(ip + 1);
        }
    }
    let leaders: Vec<_> = leaders.into_iter().collect();
    let mut blocks = Vec::with_capacity(leaders.len());
    for (index, start) in leaders.iter().copied().enumerate() {
        let end = leaders.get(index + 1).copied().unwrap_or(len);
        let last_ip = end - 1;
        let last = &function.instructions[last_ip];
        let mut successors = Vec::new();
        match last.opcode {
            20 => successors.push(jump_target(last_ip, &last.args[0])?),
            21 | 22 => {
                successors.push(jump_target(last_ip, &last.args[1])?);
                if end < len {
                    successors.push(end);
                }
            }
            26 => {}
            _ if end < len => successors.push(end),
            _ => {}
        }
        successors.sort_unstable();
        successors.dedup();
        blocks.push(BasicBlock {
            start,
            end,
            successors,
        });
    }
    Ok(ControlFlowGraph { blocks })
}

pub fn lower_to_ir(function: &Function) -> Result<FunctionIr> {
    let cfg = build_cfg(function)?;
    let instructions = function
        .instructions
        .iter()
        .cloned()
        .enumerate()
        .map(|(pc, instruction)| IrInstruction { pc, instruction })
        .collect();
    Ok(FunctionIr { cfg, instructions })
}

fn parse_header_prefix(reader: &mut Reader<'_>) -> Result<PexHeader> {
    ensure!(
        reader.u32_be()? == SKYRIM_MAGIC,
        "unsupported or invalid PEX magic"
    );
    let major_version = reader.u8()?;
    let minor_version = reader.u8()?;
    let game_id = reader.u16()?;
    ensure!(
        game_id == 1,
        "only Skyrim PEX files are supported (game id {game_id})"
    );
    Ok(PexHeader {
        major_version,
        minor_version,
        game_id,
        compilation_time: reader.u64()?,
        source_file: reader.wstring()?,
        user_name: reader.wstring()?,
        machine_name: reader.wstring()?,
        strings: vec![],
    })
}

fn skip_debug_info(reader: &mut Reader<'_>, strings: &[String]) -> Result<()> {
    if reader.u8()? == 0 {
        return Ok(());
    }
    reader.u64()?;
    for _ in 0..reader.u16()? {
        reader.string_ref(strings)?;
        reader.string_ref(strings)?;
        reader.string_ref(strings)?;
        reader.u8()?;
        for _ in 0..reader.u16()? {
            reader.u16()?;
        }
    }
    Ok(())
}

fn parse_object(reader: &mut Reader<'_>, strings: &[String]) -> Result<Object> {
    let name = reader.string_ref(strings)?;
    reader.u32()?; // serialized object size; structural reads below remain authoritative
    let parent = reader.string_ref(strings)?;
    reader.string_ref(strings)?; // doc string
    reader.u32()?; // user flags
    let auto_state = reader.string_ref(strings)?;
    let mut variables = Vec::new();
    for _ in 0..reader.u16()? {
        let name = reader.string_ref(strings)?;
        let type_name = reader.string_ref(strings)?;
        reader.u32()?;
        let default = reader.value(strings)?;
        variables.push(Variable {
            name,
            type_name,
            default,
        });
    }
    let mut properties = Vec::new();
    for _ in 0..reader.u16()? {
        let name = reader.string_ref(strings)?;
        let type_name = reader.string_ref(strings)?;
        reader.string_ref(strings)?;
        reader.u32()?;
        let flags = reader.u8()?;
        let (auto_var, read, write) = if flags & 0x04 != 0 {
            (Some(reader.string_ref(strings)?), None, None)
        } else {
            let read = if flags & 0x01 != 0 {
                Some(parse_function(reader, strings, String::new())?)
            } else {
                None
            };
            let write = if flags & 0x02 != 0 {
                Some(parse_function(reader, strings, String::new())?)
            } else {
                None
            };
            (None, read, write)
        };
        properties.push(Property {
            name,
            type_name,
            auto_var,
            readable: flags & 0x01 != 0,
            writable: flags & 0x02 != 0,
            read,
            write,
        });
    }
    let mut states = Vec::new();
    for _ in 0..reader.u16()? {
        let state_name = reader.string_ref(strings)?;
        let mut functions = Vec::new();
        for _ in 0..reader.u16()? {
            let name = reader.string_ref(strings)?;
            functions.push(parse_function(reader, strings, name)?);
        }
        states.push(State {
            name: state_name,
            functions,
        });
    }
    Ok(Object {
        name,
        parent,
        auto_state,
        variables,
        properties,
        states,
    })
}

fn parse_function(reader: &mut Reader<'_>, strings: &[String], name: String) -> Result<Function> {
    let return_type = reader.string_ref(strings)?;
    reader.string_ref(strings)?;
    reader.u32()?;
    let flags = reader.u8()?;
    let params = parse_typed_names(reader, strings)?;
    let locals = parse_typed_names(reader, strings)?;
    let count = reader.u16()? as usize;
    let mut instructions = Vec::with_capacity(count);
    for _ in 0..count {
        let opcode = reader.u8()?;
        let Some(arg_count) = OPCODE_ARGS.get(opcode as usize).copied() else {
            bail!("Fallout/unknown opcode {opcode} in Skyrim PEX")
        };
        let mut args = Vec::with_capacity(arg_count);
        for _ in 0..arg_count {
            args.push(reader.value(strings)?);
        }
        let mut varargs = Vec::new();
        if matches!(opcode, 23..=25) {
            let Value::Integer(count) = reader.value(strings)? else {
                bail!("call vararg count is not an integer")
            };
            ensure!(
                (0..=u16::MAX as i32).contains(&count),
                "invalid call vararg count {count}"
            );
            for _ in 0..count {
                varargs.push(reader.value(strings)?);
            }
        }
        instructions.push(Instruction {
            opcode,
            args,
            varargs,
        });
    }
    Ok(Function {
        name,
        return_type,
        flags,
        params,
        locals,
        instructions,
    })
}

fn parse_typed_names(reader: &mut Reader<'_>, strings: &[String]) -> Result<Vec<TypedName>> {
    let mut names = Vec::new();
    for _ in 0..reader.u16()? {
        names.push(TypedName {
            name: reader.string_ref(strings)?,
            type_name: reader.string_ref(strings)?,
        });
    }
    Ok(names)
}

/// Emits a Luau function with native-call handling or instruction dispatch in bounded chunks.
fn emit_function(out: &mut String, state: &State, function: &Function) -> Result<()> {
    let exported = if state.name.is_empty() {
        function.name.clone()
    } else {
        format!("{}::{}", state.name, function.name)
    };
    write!(out, "Script[{}] = function(self", lua_string(&exported))?;
    for index in 0..function.params.len() {
        write!(out, ", p{index}")?;
    }
    writeln!(out, ")")?;
    writeln!(out, "    local __rt = self.__runtime")?;
    if function.flags & 0x02 != 0 {
        write!(
            out,
            "    return __rt:native(self, {}, {{",
            lua_string(&function.name)
        )?;
        for index in 0..function.params.len() {
            if index > 0 {
                out.push_str(", ");
            }
            write!(out, "p{index}")?;
        }
        writeln!(out, "}})")?;
        writeln!(out, "end")?;
        return Ok(());
    }
    if function.instructions.is_empty() {
        writeln!(out, "    return nil")?;
        writeln!(out, "end")?;
        return Ok(());
    }
    let ir = lower_to_ir(function)?;
    writeln!(out, "    local __v, __locals, __types = {{}}, {{}}, {{}}")?;
    for (index, param) in function.params.iter().enumerate() {
        writeln!(
            out,
            "    __locals[{}], __types[{}], __v[{}] = true, {}, p{index}",
            lua_string(&param.name),
            lua_string(&param.name),
            lua_string(&param.name),
            lua_string(&param.type_name)
        )?;
    }
    for local in &function.locals {
        writeln!(
            out,
            "    __locals[{}], __types[{}] = true, {}",
            lua_string(&local.name),
            lua_string(&local.name),
            lua_string(&local.type_name)
        )?;
    }
    writeln!(
        out,
        "    local function __get(key) if string.lower(key) == \"self\" then return self end local k = __rt:identifier_key(__locals, key) if k then return __v[k] else return __rt:get_field(self, key) end end"
    )?;
    writeln!(
        out,
        "    local function __set(key, value) local k = __rt:identifier_key(__locals, key) if k then __v[k] = value else __rt:set_field(self, key, value) end end"
    )?;
    writeln!(out, "    local __pc = 0")?;
    writeln!(out, "    while true do")?;
    // Luau recursively parses elseif chains. Bound each instruction dispatch
    // to 128 branches without changing jump targets or the interpreter state.
    writeln!(
        out,
        "        if __pc == {} then return nil end",
        ir.instructions.len()
    )?;
    writeln!(
        out,
        "        if __pc < 0 or __pc >= {} then error(\"invalid Papyrus program counter: \" .. tostring(__pc)) end",
        ir.instructions.len()
    )?;
    for chunk in ir.instructions.chunks(128) {
        writeln!(
            out,
            "        if __pc >= {} and __pc <= {} then",
            chunk[0].pc,
            chunk.last().unwrap().pc
        )?;
        for (index, ir_instruction) in chunk.iter().enumerate() {
            let ip = ir_instruction.pc;
            writeln!(
                out,
                "            {} __pc == {ip} then",
                if index == 0 { "if" } else { "elseif" }
            )?;
            emit_instruction(out, ip, &ir_instruction.instruction)?;
        }
        writeln!(out, "            end")?;
        writeln!(out, "            continue")?;
        writeln!(out, "        end")?;
    }
    writeln!(out, "    end")?;
    writeln!(out, "end")?;
    Ok(())
}

/// Emits one instruction, preserving lexical ownership for Parent calls.
fn emit_instruction(out: &mut String, ip: usize, ins: &Instruction) -> Result<()> {
    let a = &ins.args;
    let next = ip + 1;
    let assign = |out: &mut String, dst: &Value, expression: &str| -> Result<()> {
        let Value::Identifier(name) = dst else {
            bail!("opcode {} destination is not an identifier", ins.opcode)
        };
        writeln!(out, "            __set({}, {expression})", lua_string(name))?;
        Ok(())
    };
    let binary = |out: &mut String, op: &str| -> Result<()> {
        assign(
            out,
            &a[0],
            &format!("{} {op} {}", value_expr(&a[1]), value_expr(&a[2])),
        )
    };
    match ins.opcode {
        0 => {}
        1 | 2 => binary(out, "+")?,
        3 | 4 => binary(out, "-")?,
        5 | 6 => binary(out, "*")?,
        7 | 8 => binary(out, "/")?,
        9 => binary(out, "%")?,
        10 => assign(out, &a[0], &format!("not {}", value_expr(&a[1])))?,
        11 | 12 => assign(out, &a[0], &format!("-{}", value_expr(&a[1])))?,
        13 => assign(out, &a[0], &value_expr(&a[1]))?,
        14 => {
            let Value::Identifier(destination) = &a[0] else {
                bail!("cast destination is not an identifier")
            };
            assign(
                out,
                &a[0],
                &format!(
                    "__rt:cast({}, __rt:get_field(__types, {}) or __rt:get_field(Script.__types, {}))",
                    value_expr(&a[1]),
                    lua_string(destination),
                    lua_string(destination)
                ),
            )?;
        }
        15 => binary(out, "==")?,
        16 => binary(out, "<")?,
        17 => binary(out, "<=")?,
        18 => binary(out, ">")?,
        19 => binary(out, ">=")?,
        20 => {
            writeln!(out, "            __pc = {}", jump_target(ip, &a[0])?)?;
            writeln!(out, "            continue")?;
            return Ok(());
        }
        21 | 22 => {
            let condition = value_expr(&a[0]);
            let condition = if ins.opcode == 22 {
                format!("not ({condition})")
            } else {
                condition
            };
            writeln!(
                out,
                "            if {condition} then __pc = {} else __pc = {next} end",
                jump_target(ip, &a[1])?
            )?;
            writeln!(out, "            continue")?;
            return Ok(());
        }
        23 => assign(
            out,
            &a[2],
            &format!(
                "__rt:call_method({}, {}, {})",
                value_expr(&a[1]),
                symbol_expr(&a[0]),
                values_table(&ins.varargs)
            ),
        )?,
        24 => assign(
            out,
            &a[1],
            &format!(
                "__rt:call_parent(self, {}, {}, Script.__parent)",
                symbol_expr(&a[0]),
                values_table(&ins.varargs)
            ),
        )?,
        25 => assign(
            out,
            &a[2],
            &format!(
                "__rt:call_static({}, {}, {})",
                symbol_expr(&a[0]),
                symbol_expr(&a[1]),
                values_table(&ins.varargs)
            ),
        )?,
        26 => {
            writeln!(out, "            return {}", value_expr(&a[0]))?;
            return Ok(());
        }
        27 => assign(
            out,
            &a[0],
            &format!(
                "tostring({}) .. tostring({})",
                value_expr(&a[1]),
                value_expr(&a[2])
            ),
        )?,
        28 => assign(
            out,
            &a[2],
            &format!(
                "__rt:get_property({}, {})",
                value_expr(&a[1]),
                symbol_expr(&a[0])
            ),
        )?,
        29 => writeln!(
            out,
            "            __rt:set_property({}, {}, {})",
            value_expr(&a[1]),
            symbol_expr(&a[0]),
            value_expr(&a[2])
        )?,
        30 => assign(out, &a[0], &format!("table.create({})", value_expr(&a[1])))?,
        31 => assign(out, &a[0], &format!("#{}", value_expr(&a[1])))?,
        32 => assign(
            out,
            &a[0],
            &format!("{}[{} + 1]", value_expr(&a[1]), value_expr(&a[2])),
        )?,
        33 => writeln!(
            out,
            "            {}[{} + 1] = {}",
            value_expr(&a[0]),
            value_expr(&a[1]),
            value_expr(&a[2])
        )?,
        34 => assign(
            out,
            &a[0],
            &format!(
                "__rt:array_find({}, {}, {})",
                value_expr(&a[1]),
                value_expr(&a[2]),
                value_expr(&a[3])
            ),
        )?,
        35 => assign(
            out,
            &a[0],
            &format!(
                "__rt:array_rfind({}, {}, {})",
                value_expr(&a[1]),
                value_expr(&a[2]),
                value_expr(&a[3])
            ),
        )?,
        _ => bail!("unsupported Skyrim opcode {}", ins.opcode),
    }
    writeln!(out, "            __pc = {next}")?;
    Ok(())
}

fn jump_target(ip: usize, value: &Value) -> Result<usize> {
    let Value::Integer(offset) = value else {
        bail!("jump offset is not an integer")
    };
    usize::try_from(ip as isize + *offset as isize).map_err(Into::into)
}

fn value_expr(value: &Value) -> String {
    match value {
        Value::None => "nil".into(),
        Value::Identifier(value) => format!("__get({})", lua_string(value)),
        Value::String(value) => lua_string(value),
        Value::Integer(value) => value.to_string(),
        Value::Float(value) if value.is_finite() => format!("{value:?}"),
        Value::Float(_) => "0/0".into(),
        Value::Bool(value) => value.to_string(),
    }
}

fn symbol_expr(value: &Value) -> String {
    match value {
        Value::Identifier(value) | Value::String(value) => lua_string(value),
        _ => value_expr(value),
    }
}

fn default_value_expr(value: &Value) -> String {
    match value {
        Value::Identifier(value) => {
            format!("runtime:resolve_identifier(self, {})", lua_string(value))
        }
        _ => value_expr(value),
    }
}

fn values_table(values: &[Value]) -> String {
    format!(
        "{{{}}}",
        values.iter().map(value_expr).collect::<Vec<_>>().join(", ")
    )
}

fn lua_string(value: &str) -> String {
    format!("{:?}", value)
}

struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}
impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }
    fn remaining(&self) -> usize {
        self.bytes.len() - self.offset
    }
    fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(count)
            .ok_or_else(|| color_eyre::eyre::eyre!("PEX offset overflow"))?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| color_eyre::eyre::eyre!("truncated PEX at offset {}", self.offset))?;
        self.offset = end;
        Ok(value)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u32_be(&mut self) -> Result<u32> {
        self.u32()
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_bits(self.u32()?))
    }
    fn wstring(&mut self) -> Result<String> {
        let length = self.u16()? as usize;
        let start = self.offset;
        String::from_utf8(self.take(length)?.to_vec())
            .map_err(|_| color_eyre::eyre::eyre!("invalid UTF-8 string at PEX offset {start}"))
    }
    fn string_table(&mut self) -> Result<Vec<String>> {
        let count = self.u16()? as usize;
        let mut strings = Vec::with_capacity(count);
        for _ in 0..count {
            strings.push(self.wstring()?);
        }
        Ok(strings)
    }
    fn string_ref(&mut self, strings: &[String]) -> Result<String> {
        let index = self.u16()? as usize;
        strings
            .get(index)
            .cloned()
            .ok_or_else(|| color_eyre::eyre::eyre!("invalid PEX string index {index}"))
    }
    fn value(&mut self, strings: &[String]) -> Result<Value> {
        Ok(match self.u8()? {
            0 => Value::None,
            1 => Value::Identifier(self.string_ref(strings)?),
            2 => Value::String(self.string_ref(strings)?),
            3 => Value::Integer(self.u32()? as i32),
            4 => Value::Float(self.f32()?),
            5 => Value::Bool(self.u8()? != 0),
            kind => bail!("invalid PEX value type {kind}"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_strategies::{arbitrary_bytes, config, corrupted};
    use proptest::prelude::*;

    fn minimal_pex() -> Vec<u8> {
        dummy_content::pex::minimal("TestScript").unwrap()
    }

    #[test]
    fn generated_pex_never_panics_under_truncation_or_mutation() {
        let bytes = dummy_content::pex::minimal("Hardening").unwrap();
        for length in 0..bytes.len() {
            let result = std::panic::catch_unwind(|| ScriptConverter::parse(&bytes[..length]));
            assert!(result.is_ok(), "PEX parser panicked at length {length}");
        }
        let mut rng = dummy_content::rng::Rng::new(11);
        for _ in 0..256 {
            let mut mutated = bytes.clone();
            let index = rng.next_u64() as usize % mutated.len();
            mutated[index] ^= 0xff;
            let result = std::panic::catch_unwind(|| ScriptConverter::parse(&mutated));
            assert!(result.is_ok(), "PEX parser panicked on mutation at {index}");
        }
    }

    #[test]
    fn parses_verifies_and_emits_complete_skyrim_pex() {
        let pex = ScriptConverter::parse(&minimal_pex()).unwrap();
        ScriptConverter::verify(&pex).unwrap();
        let luau = ScriptConverter::emit_luau(&pex).unwrap();
        mlua::Lua::new().load(&luau).into_function().unwrap();
        assert!(luau.contains("Script[\"Run\"]"));
    }

    #[test]
    fn converts_pex_file_to_luau_artifact() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("test.pex");
        let output = temp.path().join("scripts/test.luau");
        fs::write(&input, minimal_pex()).unwrap();
        ScriptConverter::convert_pex_to_luau(&input, &output).unwrap();
        let generated = fs::read_to_string(output).unwrap();
        assert!(generated.ends_with("return Script\n"));
    }

    #[test]
    fn builds_branching_cfg() {
        let function = Function {
            name: "f".into(),
            return_type: "None".into(),
            flags: 0,
            params: vec![],
            locals: vec![],
            instructions: vec![
                Instruction {
                    opcode: 22,
                    args: vec![Value::Bool(false), Value::Integer(2)],
                    varargs: vec![],
                },
                Instruction {
                    opcode: 26,
                    args: vec![Value::None],
                    varargs: vec![],
                },
                Instruction {
                    opcode: 26,
                    args: vec![Value::None],
                    varargs: vec![],
                },
            ],
        };
        let cfg = build_cfg(&function).unwrap();
        assert_eq!(cfg.blocks[0].successors, vec![1, 2]);
    }

    #[test]
    fn accepts_empty_functions_and_jumps_to_function_exit() {
        let empty = Function {
            name: "empty".into(),
            return_type: "None".into(),
            flags: 0,
            params: vec![],
            locals: vec![],
            instructions: vec![],
        };
        assert!(build_cfg(&empty).unwrap().blocks.is_empty());

        let jumping = Function {
            instructions: vec![Instruction {
                opcode: 20,
                args: vec![Value::Integer(1)],
                varargs: vec![],
            }],
            ..empty
        };
        assert_eq!(build_cfg(&jumping).unwrap().blocks[0].successors, vec![1]);
    }

    /// Verifies a large generated function compiles and executes jumps across dispatch chunk boundaries.
    #[test]
    fn large_dispatch_compiles_and_preserves_cross_chunk_jumps() {
        let mut instructions = vec![
            Instruction {
                opcode: 0,
                args: vec![],
                varargs: vec![]
            };
            2048
        ];
        instructions[0] = Instruction {
            opcode: 20,
            args: vec![Value::Integer(1024)],
            varargs: vec![],
        };
        instructions[1024] = Instruction {
            opcode: 21,
            args: vec![Value::Bool(true), Value::Integer(-896)],
            varargs: vec![],
        };
        instructions[128] = Instruction {
            opcode: 20,
            args: vec![Value::Integer(-1)],
            varargs: vec![],
        };
        instructions[127] = Instruction {
            opcode: 20,
            args: vec![Value::Integer(1920)],
            varargs: vec![],
        };
        instructions[2047] = Instruction {
            opcode: 26,
            args: vec![Value::Integer(42)],
            varargs: vec![],
        };
        let function = Function {
            name: "large".into(),
            return_type: "Int".into(),
            flags: 0,
            params: vec![],
            locals: vec![],
            instructions,
        };
        let mut source = String::from("local Script = {}\n");
        emit_function(
            &mut source,
            &State {
                name: String::new(),
                functions: vec![],
            },
            &function,
        )
        .unwrap();
        source.push_str("return Script.large({})\n");
        assert_eq!(mlua::Lua::new().load(&source).eval::<i32>().unwrap(), 42);
    }

    fn parse_and_emit(bytes: &[u8]) {
        if let Ok(pex) = ScriptConverter::parse(bytes)
            && ScriptConverter::verify(&pex).is_ok()
        {
            let _ = ScriptConverter::emit_luau(&pex);
        }
    }

    proptest! {
        #![proptest_config(config(256))]

        #[test]
        fn pex_never_panics_on_arbitrary_bytes(tail in arbitrary_bytes(512)) {
            // The magic and a supported version, so the tables are reached.
            let mut bytes = minimal_pex()[..8].to_vec();
            bytes.extend_from_slice(&tail);
            parse_and_emit(&bytes);
        }

        #[test]
        fn corrupted_pex_never_panics(bytes in corrupted(minimal_pex())) {
            parse_and_emit(&bytes);
        }
    }
}
