local Runtime = {}
Runtime.__index = Runtime

function Runtime.new()
    return setmetatable({ classes = {}, natives = {} }, Runtime)
end

-- Papyrus identifiers use ASCII case-insensitive lookup; data strings retain spelling.
-- Keep an exact-key fast path and inspect original keys on a case mismatch. This
-- also supports host class tables without rewriting their fields or callbacks.
function Runtime:identifier_key(target, name)
    if rawget(target, name) ~= nil then return name end
    local normalized = string.lower(name)
    for key in pairs(target) do
        if type(key) == "string" and string.lower(key) == normalized then return key end
    end
    return nil
end

function Runtime:get_field(target, name)
    local key = self:identifier_key(target, name)
    if key ~= nil then return rawget(target, key) end
    local meta = getmetatable(target)
    local index = meta and meta.__index
    if type(index) == "table" then
        key = self:identifier_key(index, name)
        if key ~= nil then return rawget(index, key) end
    end
    return nil
end

function Runtime:set_field(target, name, value)
    -- Type metadata retains declared field names even when their value is None.
    local key = self:identifier_key(target, name)
    local types = target.__types
    if key == nil and types ~= nil then key = self:identifier_key(types, name) end
    if key == nil then
        local meta = getmetatable(target)
        local index = meta and meta.__index
        if type(index) == "table" then key = self:identifier_key(index, name) end
    end
    target[key or name] = value
end

function Runtime:register_class(name, class)
    self.classes[string.lower(name)] = class
end

function Runtime:register_native(class_name, function_name, callback)
    self.natives[string.lower(class_name .. "::" .. function_name)] = callback
end

function Runtime:resolve_identifier(_target, name)
    return self.classes[string.lower(name)]
end

function Runtime:native(target, name, args)
    local class_name = target.__name or ""
    local callback = self.natives[string.lower(class_name .. "::" .. name)]
    assert(callback, "unregistered Papyrus native: " .. class_name .. "::" .. name)
    return callback(target, table.unpack(args))
end

-- Search each script's current state, then its empty state, before its parent.
-- The receiver remains the original instance when executing inherited code.
function Runtime:find_method(class, state, name)
    local seen = {}
    while class ~= nil do
        assert(not seen[class], "cyclic Papyrus parent hierarchy")
        seen[class] = true
        local callback = state ~= "" and self:get_field(class, state .. "::" .. name) or nil
        if callback == nil then callback = self:get_field(class, name) end
        if callback ~= nil then return callback end
        class = self.classes[string.lower(class.__parent or "")]
    end
    return nil
end

function Runtime:call_method(target, name, args)
    assert(target ~= nil, "Papyrus method target is None")
    local normalized = string.lower(name)
    if normalized == "gotostate" then
        self:goto_state(target, args[1] or "")
        return nil
    elseif normalized == "getstate" then
        return target.__state or target.__autoState or ""
    end
    local state = target.__state or target.__autoState or ""
    local callback = self:find_method(target, state, name)
    if callback == nil and target.dispatch ~= nil then
        return target:dispatch(name, table.unpack(args))
    end
    assert(type(callback) == "function", "unknown Papyrus method: " .. (target.__name or "") .. "::" .. name)
    return callback(target, table.unpack(args))
end

-- Emitted Parent calls supply the lexical parent's name, so a parent method's
-- own Parent call can advance through the hierarchy on a derived receiver.
function Runtime:call_parent(target, name, args, parent_name)
    local parent = parent_name or target.__parent
    local class = self.classes[string.lower(parent or "")]
    assert(class, "unknown Papyrus parent class: " .. tostring(parent))
    local state = target.__state or target.__autoState or ""
    local callback = self:find_method(class, state, name)
    assert(type(callback) == "function", "unknown parent method: " .. name)
    return callback(target, table.unpack(args))
end

function Runtime:call_static(class_name, name, args)
    local class = self.classes[string.lower(class_name)]
    assert(class, "unknown Papyrus class: " .. tostring(class_name))
    local callback = self:get_field(class, name)
    assert(type(callback) == "function", "unknown static method: " .. name)
    -- Global emitted functions need this runtime too. Use a per-call context so
    -- classes shared by multiple runtime instances keep their registration data.
    local context = setmetatable({ __runtime = self }, { __index = class })
    return callback(context, table.unpack(args))
end

function Runtime:get_property(target, name)
    assert(target ~= nil, "Papyrus property target is None")
    local getter = self:find_method(target, "", "get_" .. name)
    if type(getter) == "function" then
        return getter(target)
    end
    return self:get_field(target, name)
end

function Runtime:set_property(target, name, value)
    assert(target ~= nil, "Papyrus property target is None")
    local setter = self:find_method(target, "", "set_" .. name)
    if type(setter) == "function" then
        setter(target, value)
    else
        self:set_field(target, name, value)
    end
end

function Runtime:cast(value, type_name)
    local normalized = string.lower(type_name or "")
    if normalized == "int" or normalized == "integer" then
        if type(value) == "boolean" then
            return value and 1 or 0
        end
        return math.floor(tonumber(value) or 0)
    elseif normalized == "float" then
        if type(value) == "boolean" then
            return value and 1.0 or 0.0
        end
        return tonumber(value) or 0.0
    elseif normalized == "bool" or normalized == "boolean" then
        return not not value
    elseif normalized == "string" then
        return value == nil and "" or tostring(value)
    end
    return value
end

function Runtime:array_find(array, value, start_index)
    local first = math.max(0, start_index or 0)
    for index = first + 1, #array do
        if array[index] == value then return index - 1 end
    end
    return -1
end

function Runtime:array_rfind(array, value, start_index)
    local first = start_index
    if first == nil or first < 0 or first >= #array then first = #array - 1 end
    for index = first + 1, 1, -1 do
        if array[index] == value then return index - 1 end
    end
    return -1
end

-- Undeclared destinations are valid: preserve the supplied name and resolve
-- lifecycle handlers and subsequent calls with the usual empty-state fallback.
function Runtime:goto_state(target, state)
    assert(target ~= nil, "Papyrus state target is None")
    assert(type(state) == "string", "Papyrus state name must be a string")
    local old_state = target.__state or target.__autoState or ""
    local on_end = self:find_method(target, old_state, "OnEndState")
    if on_end ~= nil then on_end(target) end
    target.__state = state
    local on_begin = self:find_method(target, state, "OnBeginState")
    if on_begin ~= nil then on_begin(target) end
end

return Runtime
