use mlua::{Error, Function, Lua, Result, Table, VmState};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

pub const SOURCE: &str = include_str!("../../shared/src/papyrus_runtime.luau");

pub fn new_sandboxed_lua(memory_limit: usize, interrupt_budget: u64) -> Result<Lua> {
    let lua = Lua::new();
    lua.set_memory_limit(memory_limit)?;
    lua.sandbox(true)?;
    let remaining = Arc::new(AtomicU64::new(interrupt_budget));
    lua.set_interrupt(move |_| {
        let exhausted = remaining
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_sub(1)
            })
            .is_err();
        if exhausted {
            Err(Error::RuntimeError(
                "Papyrus script execution budget exceeded".into(),
            ))
        } else {
            Ok(VmState::Continue)
        }
    });
    Ok(lua)
}

pub fn load_runtime(lua: &Lua) -> Result<Table> {
    let module: Table = lua.load(SOURCE).set_name("@papyrus_runtime.luau").eval()?;
    module.get::<Function>("new")?.call(())
}

/// Registers a class through the shared case-insensitive Papyrus name resolver.
pub fn register_class(runtime: &Table, name: &str, class: Table) -> Result<()> {
    let register: Function = runtime.get("register_class")?;
    register.call((runtime.clone(), name, class))
}

/// Registers native names with the same case-insensitive policy as emitted calls.
pub fn register_native(
    runtime: &Table,
    class_name: &str,
    function_name: &str,
    callback: Function,
) -> Result<()> {
    let register: Function = runtime.get("register_native")?;
    register.call((runtime.clone(), class_name, function_name, callback))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_executes_calls_properties_arrays_and_natives() {
        let lua = new_sandboxed_lua(8 * 1024 * 1024, 10_000).unwrap();
        let runtime = load_runtime(&lua).unwrap();
        let class: Table = lua
            .load(
                r#"return {
                    __name = "Test",
                    Add = function(self, a, b) return a + b end,
                    get_Value = function(self) return self.value end,
                    set_Value = function(self, value) self.value = value end,
                }"#,
            )
            .eval()
            .unwrap();
        register_class(&runtime, "Test", class.clone()).unwrap();
        let call_method: Function = runtime.get("call_method").unwrap();
        let args = lua.create_sequence_from([2, 3]).unwrap();
        let sum: i64 = call_method
            .call((runtime.clone(), class.clone(), "Add", args))
            .unwrap();
        assert_eq!(sum, 5);

        let find: Function = runtime.get("array_find").unwrap();
        let values = lua.create_sequence_from([10, 20, 30]).unwrap();
        let index: i64 = find.call((runtime.clone(), values, 20, 0)).unwrap();
        assert_eq!(index, 1);

        let cast: Function = runtime.get("cast").unwrap();
        let int_val: i64 = cast.call((runtime.clone(), true, "int")).unwrap();
        assert_eq!(int_val, 1);
        let float_val: f64 = cast.call((runtime.clone(), true, "float")).unwrap();
        assert_eq!(float_val, 1.0);

        let native = lua
            .create_function(|_, (_target, value): (Table, i64)| Ok(value * 2))
            .unwrap();
        register_native(&runtime, "Test", "Double", native).unwrap();
        let invoke: Function = runtime.get("native").unwrap();
        let args = lua.create_sequence_from([21]).unwrap();
        let doubled: i64 = invoke.call((runtime, class, "Double", args)).unwrap();
        assert_eq!(doubled, 42);
    }

    /// Rust registrations retain host tables and resolve names independently of their spelling.
    #[test]
    fn host_registration_resolves_mixed_case_names() {
        let lua = new_sandboxed_lua(8 * 1024 * 1024, 10_000).unwrap();
        let runtime = load_runtime(&lua).unwrap();
        let class: Table = lua
            .load(
                r#"return {
            __name = 'HostClass', __parent = '', __autoState = '', Flag = false,
            ['Busy::Value'] = function(self) return 47 end,
            Value = function(self) return 11 end,
            get_Counter = function(self) return self.counter end,
            set_Counter = function(self, value) self.counter = value end,
        }"#,
            )
            .eval()
            .unwrap();
        register_class(&runtime, "HOSTCLASS", class.clone()).unwrap();
        let callback = lua
            .create_function(|_, (_target, value): (Table, i64)| Ok(value + 3))
            .unwrap();
        register_native(&runtime, "hostclass", "DOUBLE", callback).unwrap();
        lua.globals().set("rt", runtime).unwrap();
        lua.globals().set("Class", class).unwrap();
        lua.load(
            r#"
            assert(rt:resolve_identifier(nil, 'hOsTcLaSs') == Class)
            local a = setmetatable({}, { __index = Class })
            rt:call_method(a, 'gOtOsTaTe', {'bUsY'})
            assert(rt:call_method(a, 'gEtStAtE', {}) == 'bUsY')
            assert(rt:call_method(a, 'vAlUe', {}) == 47)
            assert(rt:native(a, 'dOuBlE', {70}) == 73)
            assert(rt:call_static('hostclass', 'VALUE', {}) == 11)
            rt:set_property(a, 'COUNTER', 89)
            assert(rt:get_property(a, 'counter') == 89)
            rt:set_property(a, 'FLAG', true)
            assert(a.Flag == true and a.FLAG == nil and Class.Flag == false)
            assert(Class['Busy::Value'] ~= nil and Class.Value ~= nil, 'host keys retained')
        "#,
        )
        .exec()
        .unwrap();
    }

    #[test]
    fn sandbox_stops_unbounded_scripts() {
        let lua = new_sandboxed_lua(2 * 1024 * 1024, 10).unwrap();
        let error = lua.load("while true do end").exec().unwrap_err();
        assert!(error.to_string().contains("budget exceeded"));
    }
}
