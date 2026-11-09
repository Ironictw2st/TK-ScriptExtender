-- se_api.lua : Lua face of the TW:3K script extender DLL (embedded in the DLL, run once per
-- Lua state right after the se_* natives are registered; a console dofile() of a newer copy
-- simply redefines it).
--
--   se.version()                       DLL version string
--   se.available(native_name)          true when a se_* native exists in this state
--   se.query.*                         read-only, return plain values / tables or nil, message
--   se.modify.*                        mutate; run on the model thread (multiplayer: only inside
--                                      model callbacks, never queued, see on_model);
--                                      return ok, message (ok = true means queued or done)
--
-- Lua 5.1. Every engine call is pcall-guarded because null interfaces answer any method with
-- a function and list interfaces have no is_null_interface.

se = se or {}
se.query = se.query or {}
se.modify = se.modify or {}

-- Logging: se.logger (set it to ModLog from a console script), else a global ModLog, else the
-- DLL's se_log (script_extender.log). The chunk's own environment usually has no ModLog.
local function log(s)
	s = "[se] " .. tostring(s)
	if type(se.logger) == "function" then pcall(se.logger, s) return end
	if type(ModLog) == "function" then pcall(ModLog, s) return end
	if type(se_log) == "function" then se_log(s) end
end
se.log = log

local function is_null(v)
	if type(v) ~= "userdata" then return true end
	local ok, r = pcall(function() return v:is_null_interface() end)
	return ok and r == true
end
se.is_null = is_null

local function num(v) return tonumber(v) end
local function str(v) return tostring(v) end

function se.version()
	if type(se_version) == "function" then return se_version() end
	return "unknown (DLL older than 0.8)"
end

-- Globals are looked up through the state's real global table (getfenv(0)); a console
-- sandbox may give this chunk a different _G.
local ENV = nil
pcall(function() ENV = getfenv(1) end)
if type(ENV) ~= "table" then pcall(function() ENV = getfenv(0) end) end
if type(ENV) ~= "table" then ENV = _G end
local function G(name)
	local v = ENV[name]
	if v == nil and type(_G) == "table" then v = _G[name] end
	return v
end
se.G = G

function se.available(name)
	return type(G(name)) == "function"
end

-- se.status() -> { state = "ready"|"booting"|"refused"|"not_game", version = "<dll>.<sync>",
-- patches = n }. The same values the DLL exports to other native mods (se_status / se_version).
function se.status()
	local f = G("se_status")
	if type(f) ~= "function" then return { state = "unknown", version = se.version(), patches = 0 } end
	local st, v, n = f()
	local names = { [0] = "booting", [1] = "ready", [2] = "refused", [3] = "not_game" }
	return { state = names[st] or tostring(st), version = v, patches = n }
end

local function need(...)
	for _, n in ipairs({ ... }) do
		if type(G(n)) ~= "function" then return false, "native " .. n .. " is not available (DLL too old or not injected)" end
	end
	return true
end

local function cm_()
	local cm = G("cm")
	if not cm then return nil, "campaign manager (cm) is not available in this Lua state" end
	return cm
end

-- Run f on the model thread. When the caller already may modify (inside an event handler) f
-- runs at once and its result is returned; otherwise it is queued through wait_for_model_sp and
-- the result goes to the optional callback and the log.
local function on_model(what, f, cb)
	local cm, err = cm_()
	if not cm then return false, err end
	-- Multiplayer campaigns are lockstep: a change has to happen on every machine at the same
	-- model tick. Inside a model callback (event listeners, first tick, turn start ...) the same
	-- script runs everywhere, so the call is executed. Outside of one (UI clicks, the console,
	-- timers) only this machine would change its model, so the call is refused instead of
	-- being queued (wait_for_model_sp is single-player only).
	-- cm:is_multiplayer() is a value the campaign manager stashes on WorldCreated and reads false
	-- before it, so the model is asked first; if neither answers, the machine is treated as MP
	-- (refuse rather than queue on one peer).
	local ok_mp, mp = pcall(function() return cm:query_model():is_multiplayer() end)
	if not ok_mp or type(mp) ~= "boolean" then
		ok_mp, mp = pcall(function() return cm:is_multiplayer() end)
		if not ok_mp or type(mp) ~= "boolean" then mp = true end
	end
	local ok_can, can = pcall(function() return cm:can_modify() end)
	if mp == true and not (ok_can and can == true) then
		return false, "refused in multiplayer: " .. what .. " was called outside a model callback (it would only run on this machine and desync)"
	end
	if ok_can and can == true then
		local ok, msg = f()
		log(what .. " -> " .. str(ok) .. " : " .. str(msg))
		if cb then cb(ok, msg) end
		return ok, msg
	end
	cm:wait_for_model_sp(function()
		local okp, ok, msg = pcall(f)
		if not okp then ok, msg = false, "error: " .. str(ok) end
		log(what .. " -> " .. str(ok) .. " : " .. str(msg))
		if cb then cb(ok, msg) end
	end)
	return true, what .. " queued on the model thread (result in the log / callback)"
end
se.on_model = on_model

----------------------------------------------------------------------------------------------
-- characters
----------------------------------------------------------------------------------------------

function se.character(cqi)
	local cm, err = cm_()
	if not cm then return nil, err end
	cqi = num(cqi)
	if not cqi then return nil, "cqi must be a number" end
	local ok, q = pcall(function() return cm:query_character(cqi) end)
	if not ok or is_null(q) then return nil, "no character with cqi " .. cqi end
	local okc, real = pcall(function() return q:command_queue_index() end)
	if not okc or num(real) ~= cqi then return nil, "cqi mismatch for " .. cqi end
	return q
end

-- se.query.character(cqi) -> table | nil, message
function se.query.character(cqi)
	local q, err = se.character(cqi)
	if not q then return nil, err end
	local function g(name)
		local ok, v = pcall(function() return q[name](q) end)
		if ok then return v end
		return nil
	end
	local t = {
		cqi = num(cqi),
		template = str(g("generation_template_key")),
		faction = (not is_null(g("faction"))) and str(g("faction"):name()) or "",
		in_pool = g("is_character_is_faction_recruitment_pool") == true,
		recruited = g("is_character_in_faction_recruited_characters") == true,
		rank = num(g("rank")),
		experience = num(g("current_experience")),
		is_faction_leader = g("is_faction_leader") == true,
		has_military_force = g("has_military_force") == true,
		has_region = g("has_region") == true,
		wounded = g("is_wounded") == true,
	}
	if se.available("se_char_info") then t.engine = str(se_char_info(q, num(cqi))) end
	return t
end

-- se.query.pool_lock(cqi) -> status, counter | nil, message   (10/0 = available, 5/N = locked)
function se.query.pool_lock(cqi)
	local okn, err = need("se_pool_lock_get")
	if not okn then return nil, err end
	local q, e2 = se.character(cqi)
	if not q then return nil, e2 end
	local s, c = se_pool_lock_get(q, num(cqi))
	if s == nil then return nil, str(c) end
	return num(s), num(c)
end

-- se.modify.pool_lock(cqi, turns) : lock the pool character for `turns` rounds (status 5)
function se.modify.pool_lock(cqi, turns)
	local okn, err = need("se_pool_lock_set")
	if not okn then return false, err end
	turns = num(turns) or 1
	return on_model("pool_lock(" .. str(cqi) .. ", " .. turns .. ")", function()
		local q, e2 = se.character(cqi)
		if not q then return false, e2 end
		return se_pool_lock_set(q, num(cqi), 5, turns)
	end)
end

-- se.modify.pool_unlock(cqi) : make the pool character available now (status 10, counter 0)
function se.modify.pool_unlock(cqi)
	local okn, err = need("se_pool_lock_set")
	if not okn then return false, err end
	return on_model("pool_unlock(" .. str(cqi) .. ")", function()
		local q, e2 = se.character(cqi)
		if not q then return false, e2 end
		return se_pool_lock_set(q, num(cqi), 10, 0)
	end)
end

-- se.modify.release_to_pool(cqi) : recruited -> own faction's recruitment pool
function se.modify.release_to_pool(cqi)
	local okn, err = need("se_release_to_pool")
	if not okn then return false, err end
	return on_model("release_to_pool(" .. str(cqi) .. ")", function()
		local q, e2 = se.character(cqi)
		if not q then return false, e2 end
		return se_release_to_pool(q, num(cqi))
	end)
end

-- se.modify.move_character(cqi, faction_key, state)
--   state = "pool" (default) | "recruited". Moves the character to faction_key if needed, then
--   forces the requested list membership. The release step runs one second after the move so
--   the engine has relinked the character first.
function se.modify.move_character(cqi, faction_key, state)
	state = state or "pool"
	if state ~= "pool" and state ~= "recruited" then return false, "state must be 'pool' or 'recruited'" end
	if state == "pool" then
		local okn, err = need("se_release_to_pool")
		if not okn then return false, err end
	end
	local cm, err = cm_()
	if not cm then return false, err end
	local tag = "move_character(" .. str(cqi) .. ", " .. str(faction_key) .. ", " .. state .. ")"
	return on_model(tag, function()
		local q, e2 = se.character(cqi)
		if not q then return false, e2 end
		local here = str(q:faction():name())
		local m = cm:modify_character(num(cqi))
		if is_null(m) then return false, "modify_character returned null" end
		local moved = false
		if here ~= faction_key then
			local okf, f = pcall(function() return cm:query_faction(faction_key) end)
			if not okf or is_null(f) then return false, "no faction '" .. str(faction_key) .. "'" end
			m:move_to_faction(faction_key)
			moved = true
		end
		local function finish()
			local q2 = se.character(cqi)
			if not q2 then log(tag .. ": character vanished after the move") return end
			local in_pool = q2:is_character_is_faction_recruitment_pool()
			if state == "pool" and not in_pool then
				local ok, msg = se_release_to_pool(q2, num(cqi))
				log(tag .. ": release -> " .. str(ok) .. " : " .. str(msg))
			elseif state == "recruited" and in_pool then
				cm:modify_character(num(cqi)):move_recruitment_pool_character_to_recruited_characters()
				log(tag .. ": moved pool -> recruited")
			else
				log(tag .. ": already in the requested state")
			end
			local q3 = se.query.character(cqi)
			if q3 then log(tag .. ": now faction=" .. q3.faction .. " in_pool=" .. str(q3.in_pool) .. " recruited=" .. str(q3.recruited)) end
		end
		if moved then
			cm:callback(function() cm:wait_for_model_sp(finish) end, 1.0)
			return true, "moved " .. here .. " -> " .. faction_key .. "; state step follows in 1 s"
		end
		finish()
		return true, "state step done"
	end)
end

----------------------------------------------------------------------------------------------
-- retinues and units
----------------------------------------------------------------------------------------------

-- Ordered slot list of the character's commanded persistent retinue.
local function slots_of(q)
	local ok, ret = pcall(function() return q:commanded_persistent_retinue() end)
	if not ok or is_null(ret) then return nil, "character commands no persistent retinue" end
	local okl, slots = pcall(function() return ret:retinue_slots() end)
	if not okl then return nil, "retinue_slots failed" end
	local list = {}
	local n = num(slots:num_items()) or 0
	for i = 0, n - 1 do
		local s = slots:item_at(i)
		local oki, idx = pcall(function() return s:slot_index() end)
		local okk, key = pcall(function() return s:slot_unit_record_key() end)
		list[#list + 1] = { slot = s, index = oki and num(idx) or -1, unit_key = okk and str(key) or "" }
	end
	table.sort(list, function(a, b) return a.index < b.index end)
	return list, ret
end

local function slot_by_index(q, index)
	local list, err = slots_of(q)
	if not list then return nil, err end
	for _, e in ipairs(list) do
		if e.index == num(index) then return e end
	end
	return nil, "slot " .. str(index) .. " not found"
end

-- QUERY_UNIT of a slot through the military force link (nil when the army is not on the map).
local function unit_of_slot(entry)
	local okm, ms = pcall(function() return entry.slot:linked_to_military_force_retinue_slot() end)
	if not okm or is_null(ms) then return nil, "slot is not linked to a military force slot (army not deployed?)" end
	local oku, u = pcall(function() return ms:linked_to_unit() end)
	if not oku or is_null(u) then return nil, "slot has no linked unit" end
	return u
end

local function parse_items(s)
	local items = {}
	for line in str(s):gmatch("[^\n]+") do
		local key, rec, cost, turns, reasons = line:match("^%s+(%S+) rec=(%S+) cost=(%d+) turns=(%d+) reasons=(%S+)")
		if key then
			items[#items + 1] = { key = key, record = rec, cost = num(cost), turns = num(turns), reasons = num(reasons) or 0 }
		end
	end
	return items
end

-- se.query.retinue(cqi) -> { {index, slot_cqi, unit_key, strength, experience, can_recruit, is_recruiting, recruiting}, ... }
function se.query.retinue(cqi)
	local q, err = se.character(cqi)
	if not q then return nil, err end
	local list, e2 = slots_of(q)
	if not list then return nil, e2 end
	local out = {}
	for _, e in ipairs(list) do
		local row = { index = e.index, unit_key = e.unit_key }
		-- cqi of the persistent slot = what UnitRecruitmentInitiated reports as context:cqi()
		local okq, scqi = pcall(function() return e.slot:command_queue_index() end)
		row.slot_cqi = okq and num(scqi) or nil
		local u = unit_of_slot(e)
		if u then
			local oks, st = pcall(function() return u:percentage_proportion_of_full_strength() end)
			local okx, xp = pcall(function() return u:experience_level() end)
			row.strength = oks and num(st) or nil
			row.experience = okx and num(xp) or nil
		end
		local oki, ri = pcall(function() return e.slot:recruitment_interface() end)
		if oki and not is_null(ri) then
			local okc, can = pcall(function() return ri:can_recruit() end)
			local okr, rec = pcall(function() return ri:is_recruiting() end)
			local okk, rk = pcall(function() return ri:recruiting_unit_key() end)
			row.can_recruit = okc and can == true
			row.is_recruiting = okr and rec == true
			row.recruiting = okk and str(rk) or ""
		end
		out[#out + 1] = row
	end
	return out
end

-- se.query.recruitable(cqi, slot_index) -> { {key, cost, turns, reasons}, ... }  (reasons 0 = unlocked)
function se.query.recruitable(cqi, slot_index)
	local okn, err = need("se_slot_items")
	if not okn then return nil, err end
	local q, e1 = se.character(cqi)
	if not q then return nil, e1 end
	local e, e2 = slot_by_index(q, slot_index)
	if not e then return nil, e2 end
	local oki, ri = pcall(function() return e.slot:recruitment_interface() end)
	if not oki or is_null(ri) then return nil, "slot " .. str(slot_index) .. " has no recruitment interface" end
	return parse_items(se_slot_items(ri))
end

-- se.query.unit(cqi, slot_index) -> { unit_key, strength, experience, engine } | nil, message
function se.query.unit(cqi, slot_index)
	local q, err = se.character(cqi)
	if not q then return nil, err end
	local e, e2 = slot_by_index(q, slot_index)
	if not e then return nil, e2 end
	local u, e3 = unit_of_slot(e)
	if not u then return nil, e3 end
	local t = { unit_key = str(u:unit_key()), strength = num(u:percentage_proportion_of_full_strength()), experience = num(u:experience_level()) }
	if se.available("se_unit_info") then t.engine = str(se_unit_info(u)) end
	if se.available("se_unit_strength_get") then t.strength_engine = se_unit_strength_get(u) end
	return t
end

-- Apply the post-recruit options (hp, experience) to the unit now sitting in the slot.
local function apply_unit_opts(q, slot_index, opts, tag)
	local e = slot_by_index(q, slot_index)
	if not e then log(tag .. ": slot " .. str(slot_index) .. " vanished") return end
	log(tag .. ": after 1 s slot " .. str(slot_index) .. " unit=" .. e.unit_key)
	if opts.hp == nil and opts.experience == nil then return end
	local u, err = unit_of_slot(e)
	if not u then log(tag .. ": cannot apply hp/experience: " .. err) return end
	if opts.hp ~= nil then
		if se.available("se_unit_strength_set") then
			local ok, msg = se_unit_strength_set(u, num(opts.hp))
			log(tag .. ": strength " .. str(opts.hp) .. "% -> " .. str(ok) .. " : " .. str(msg))
		else
			log(tag .. ": hp option ignored: se_unit_strength_set is not in this DLL yet")
		end
	end
	if opts.experience ~= nil then
		if se.available("se_unit_xp_set") then
			local ok, msg = se_unit_xp_set(u, num(opts.experience))
			log(tag .. ": experience " .. str(opts.experience) .. " -> " .. str(ok) .. " : " .. str(msg))
		else
			log(tag .. ": experience option ignored: se_unit_xp_set is not in this DLL yet")
		end
	end
end

-- se.modify.recruit(cqi, unit_key, opts)
--   opts.source  "unlocked" (default: refuse a locked item) | "locked" | "any" (force through)
--   opts.free    true = zero the cost
--   opts.slot    slot index to recruit INTO (default: first empty slot that can recruit)
--   opts.replace true = allow an occupied slot (the unit there is replaced)
--   opts.hp      0..100 strength percent applied 1 s after the recruit (needs se_unit_strength_set)
--   opts.experience  unit experience applied 1 s after the recruit (needs se_unit_xp_set)
function se.modify.recruit(cqi, unit_key, opts)
	opts = opts or {}
	local okn, err = need("se_recruit_unit", "se_slot_items")
	if not okn then return false, err end
	if type(unit_key) ~= "string" or unit_key == "" then return false, "unit_key must be a non-empty string" end
	local source = opts.source or "unlocked"
	if source ~= "unlocked" and source ~= "locked" and source ~= "any" then return false, "opts.source must be 'unlocked', 'locked' or 'any'" end
	if opts.hp ~= nil and (num(opts.hp) == nil or num(opts.hp) < 0 or num(opts.hp) > 100) then return false, "opts.hp must be 0..100" end
	local cm, e0 = cm_()
	if not cm then return false, e0 end
	local tag = "recruit(" .. str(cqi) .. ", " .. unit_key .. ")"
	return on_model(tag, function()
		local q, e1 = se.character(cqi)
		if not q then return false, e1 end
		local list, e2 = slots_of(q)
		if not list then return false, e2 end
		local chosen
		if opts.slot ~= nil then
			for _, e in ipairs(list) do if e.index == num(opts.slot) then chosen = e end end
			if not chosen then return false, "slot " .. str(opts.slot) .. " not found" end
			if chosen.index == 0 then return false, "slot 0 is the commander" end
			if chosen.unit_key ~= "" and not opts.replace then return false, "slot " .. str(opts.slot) .. " holds " .. chosen.unit_key .. "; pass replace = true to swap it" end
		else
			for _, e in ipairs(list) do
				if not chosen and e.index > 0 and e.unit_key == "" then
					local oki, ri = pcall(function() return e.slot:recruitment_interface() end)
					if oki and not is_null(ri) and ri:can_recruit() == true then chosen = e end
				end
			end
			if not chosen and opts.replace then
				for _, e in ipairs(list) do
					if not chosen and e.index > 0 then
						local oki, ri = pcall(function() return e.slot:recruitment_interface() end)
						if oki and not is_null(ri) and ri:can_recruit() == true then chosen = e end
					end
				end
			end
			if not chosen then return false, "no empty slot that can recruit (pass opts.slot, or replace = true)" end
		end
		local oki, ri = pcall(function() return chosen.slot:recruitment_interface() end)
		if not oki or is_null(ri) then return false, "slot " .. chosen.index .. " has no recruitment interface" end
		local items = parse_items(se_slot_items(ri))
		local item
		for _, it in ipairs(items) do if it.key == unit_key then item = it end end
		if not item then return false, unit_key .. " is not in slot " .. chosen.index .. "'s item list (" .. #items .. " items)" end
		if source == "unlocked" and item.reasons ~= 0 then
			return false, unit_key .. " is locked (reasons " .. string.format("0x%x", item.reasons) .. "); use opts.source = 'locked' or 'any'"
		end
		if source == "locked" and item.reasons == 0 then
			return false, unit_key .. " is not locked; use opts.source = 'unlocked' or 'any'"
		end
		local mode = 0
		if item.reasons ~= 0 then mode = mode + 1 end
		if opts.free then mode = mode + 2 end
		local ok, msg = se_recruit_unit(ri, unit_key, mode)
		if ok then
			local idx = chosen.index
			cm:callback(function() apply_unit_opts(q, idx, opts, tag) end, 1.0)
			return true, "slot " .. idx .. (chosen.unit_key ~= "" and (" (replacing " .. chosen.unit_key .. ")") or "") .. ": " .. str(msg)
		end
		return false, str(msg)
	end)
end

-- se.modify.replace(cqi, slot_index, unit_key, opts) : recruit INTO an occupied slot
function se.modify.replace(cqi, slot_index, unit_key, opts)
	opts = opts or {}
	opts.slot = slot_index
	opts.replace = true
	return se.modify.recruit(cqi, unit_key, opts)
end

-- se.modify.disband(cqi, slot_index) : empty that retinue slot (never slot 0). This is what the
-- UI's Disband does: the slot's recruit command with an empty unit key, which the engine
-- resolves to the slot's "empty" item. (CCQ_DISBAND_UNIT refuses retinue-slot units.)
function se.modify.disband(cqi, slot_index)
	local okn, err = need("se_recruit_unit")
	if not okn then return false, err end
	if num(slot_index) == 0 then return false, "refusing to disband the commander's own slot" end
	local cm, e0 = cm_()
	if not cm then return false, e0 end
	local tag = "disband(" .. str(cqi) .. ", " .. str(slot_index) .. ")"
	return on_model(tag, function()
		local q, e1 = se.character(cqi)
		if not q then return false, e1 end
		local e, e2 = slot_by_index(q, slot_index)
		if not e then return false, e2 end
		if e.unit_key == "" then return false, "slot " .. str(slot_index) .. " is already empty" end
		local oki, ri = pcall(function() return e.slot:recruitment_interface() end)
		if not oki or is_null(ri) then return false, "slot " .. str(slot_index) .. " has no recruitment interface" end
		local u = unit_of_slot(e)
		if u and se.available("se_unit_info") then log(tag .. ": " .. str(se_unit_info(u))) end
		local ok, msg = se_recruit_unit(ri, "", 1)
		if ok then
			cm:callback(function()
				local e4 = slot_by_index(q, slot_index)
				log(tag .. ": after 1 s slot " .. str(slot_index) .. " unit=" .. (e4 and e4.unit_key or "?"))
			end, 1.0)
		end
		return ok, msg
	end)
end

-- se.modify.unit_strength(cqi, slot_index, percent) / se.modify.unit_experience(cqi, slot_index, level)
function se.modify.unit_strength(cqi, slot_index, percent)
	local okn, err = need("se_unit_strength_set")
	if not okn then return false, err end
	return on_model("unit_strength(" .. str(cqi) .. ", " .. str(slot_index) .. ", " .. str(percent) .. ")", function()
		local q, e1 = se.character(cqi)
		if not q then return false, e1 end
		local e, e2 = slot_by_index(q, slot_index)
		if not e then return false, e2 end
		local u, e3 = unit_of_slot(e)
		if not u then return false, e3 end
		return se_unit_strength_set(u, num(percent))
	end)
end

function se.modify.unit_experience(cqi, slot_index, level)
	local okn, err = need("se_unit_xp_set")
	if not okn then return false, err end
	return on_model("unit_experience(" .. str(cqi) .. ", " .. str(slot_index) .. ", " .. str(level) .. ")", function()
		local q, e1 = se.character(cqi)
		if not q then return false, e1 end
		local e, e2 = slot_by_index(q, slot_index)
		if not e then return false, e2 end
		local u, e3 = unit_of_slot(e)
		if not u then return false, e3 end
		return se_unit_xp_set(u, num(level))
	end)
end

----------------------------------------------------------------------------------------------
-- factions: progression level / world leaders (the three-kingdoms "emperors")
----------------------------------------------------------------------------------------------

function se.faction(key)
	local cm, err = cm_()
	if not cm then return nil, err end
	if type(key) ~= "string" or key == "" then return nil, "faction key must be a non-empty string" end
	local ok, f = pcall(function() return cm:query_faction(key) end)
	if not ok or is_null(f) then return nil, "no faction '" .. key .. "'" end
	return f
end

-- se.query.faction(key) -> { key, level, max_level, level_key, is_world_leader, locked, is_human, is_dead, leaders }
function se.query.faction(key)
	local f, err = se.faction(key)
	if not f then return nil, err end
	local function g(name)
		local ok, v = pcall(function() return f[name](f) end)
		if ok then return v end
		return nil
	end
	local t = {
		key = key,
		level = num(g("progression_level")),
		max_level = num(g("max_progression_level")),
		level_key = str(g("progression_level_key")),
		is_world_leader = g("is_world_leader") == true,
		world_leader_regions = num(g("number_of_world_leader_regions")),
		is_human = g("is_human") == true,
		is_dead = g("is_dead") == true,
	}
	if se.available("se_faction_progression_get") then
		local lvl, mx, lkey, leader, locked = se_faction_progression_get(f)
		if lvl ~= nil then
			t.engine_level, t.engine_max, t.engine_level_key, t.engine_leader, t.locked = num(lvl), num(mx), str(lkey), leader == true, locked == true
		else
			t.engine_error = str(mx)
		end
	end
	if se.available("se_world_leaders") then t.leaders = str(se_world_leaders(f)) end
	return t
end

-- se.query.world_leaders() -> { faction_key, ... } (stock is_world_leader over every faction)
function se.query.world_leaders()
	local cm, err = cm_()
	if not cm then return nil, err end
	local out = {}
	local ok, list = pcall(function() return cm:query_model():world():faction_list() end)
	if not ok then return nil, "faction_list failed" end
	for i = 0, list:num_items() - 1 do
		local f = list:item_at(i)
		local okw, w = pcall(function() return f:is_world_leader() end)
		if okw and w == true then out[#out + 1] = str(f:name()) end
	end
	return out
end

-- se.modify.faction_progression(key, level) : raise a faction to progression level `level`
-- through the engine's own unlock+process path (max level = emperor -> world leader seat).
function se.modify.faction_progression(key, level)
	local okn, err = need("se_faction_progression_set")
	if not okn then return false, err end
	return on_model("faction_progression(" .. str(key) .. ", " .. str(level) .. ")", function()
		local f, e1 = se.faction(key)
		if not f then return false, e1 end
		return se_faction_progression_set(f, num(level))
	end)
end

-- se.modify.force_three_kingdoms(opts)
--   opts.forced  = { faction keys that must take a seat (in order) }
--   opts.banned  = { faction keys that never take a seat }
--   opts.include_human = true to let the human faction be auto-picked (forced list always applies)
--   opts.seats   = number of seats to fill (default: the engine's max, 3)
--   opts.bypass  = false to NOT force a seat when the engine refuses one after the level change
--                  (default: seat it anyway through the world-leader manager)
-- Fills the world-leader seats: forced factions first, then the highest-progression living
-- factions not banned. Each pick is raised to its max progression level.
function se.modify.force_three_kingdoms(opts)
	opts = opts or {}
	local okn, err = need("se_faction_progression_set", "se_faction_progression_get")
	if not okn then return false, err end
	local cm, e0 = cm_()
	if not cm then return false, e0 end
	local banned = {}
	for _, k in ipairs(opts.banned or {}) do banned[k] = true end
	return on_model("force_three_kingdoms", function()
		local list = cm:query_model():world():faction_list()
		local current, cands = {}, {}
		local seats = num(opts.seats)
		for i = 0, list:num_items() - 1 do
			local f = list:item_at(i)
			local name = str(f:name())
			local info = se.query.faction(name)
			if info then
				if not seats and info.leaders then seats = num(info.leaders:match("/(%d+)")) end
				if info.is_world_leader then current[#current + 1] = name end
				if not info.is_world_leader and not info.is_dead and not banned[name] and (opts.include_human or not info.is_human) then
					cands[#cands + 1] = { key = name, level = info.level or 0 }
				end
			end
		end
		seats = seats or 3
		local picks = {}
		for _, k in ipairs(opts.forced or {}) do
			local info = se.query.faction(k)
			if not info then return false, "forced faction '" .. str(k) .. "' does not exist" end
			if not info.is_world_leader then picks[#picks + 1] = k end
		end
		table.sort(cands, function(a, b) return a.level > b.level end)
		for _, c in ipairs(cands) do
			local dup = false
			for _, p in ipairs(picks) do if p == c.key then dup = true end end
			if not dup and #current + #picks < seats then picks[#picks + 1] = c.key end
		end
		if #current + #picks > seats then
			return false, "would need " .. (#current + #picks) .. " seats but only " .. seats .. " exist"
		end
		local report = { "seats " .. #current .. "/" .. seats .. " before; picks: " .. table.concat(picks, ", ") }
		for _, k in ipairs(picks) do
			local f = se.faction(k)
			local lvl, mx = se_faction_progression_get(f)
			local ok, msg = true, "already at level " .. str(lvl)
			if num(lvl) < num(mx) then ok, msg = se_faction_progression_set(f, num(mx)) end
			report[#report + 1] = k .. " -> " .. str(ok) .. " : " .. str(msg)
			local _, _, _, leader2 = se_faction_progression_get(f)
			if ok and leader2 ~= true and opts.bypass ~= false and se.available("se_world_leader_force") then
				local ok2, msg2 = se_world_leader_force(f)
				report[#report + 1] = k .. " seat (bypassing eligibility) -> " .. str(ok2) .. " : " .. str(msg2)
			end
		end
		return true, table.concat(report, "\n")
	end)
end

-- se.modify.world_leader(key) : give a faction an emperor seat directly (needs a free seat and a
-- capital); bypasses the engine's eligibility check.
function se.modify.world_leader(key)
	local okn, err = need("se_world_leader_force")
	if not okn then return false, err end
	return on_model("world_leader(" .. str(key) .. ")", function()
		local f, e1 = se.faction(key)
		if not f then return false, e1 end
		return se_world_leader_force(f)
	end)
end

-- se.modify.emperor_policy(policy) : persistent steering of who may become emperor.
--   policy = { forced = {...}, banned = {...} } or nil to clear. Saved with cm:save_named_value
--   and enforced on FactionTurnStart: banned factions one step below their max level get
--   lock_progression_level_changes(); forced factions are promoted while seats are free.
function se.modify.emperor_policy(policy)
	local cm, e0 = cm_()
	if not cm then return false, e0 end
	se._emperor_policy = policy
	local enc = ""
	if policy then enc = table.concat(policy.forced or {}, ",") .. "|" .. table.concat(policy.banned or {}, ",") end
	pcall(function() cm:save_named_value("se_emperor_policy", enc) end)
	-- The event manager is a script-side global; console scripts hand it over as se.core.
	local core = se.core or G("core")
	if type(core) ~= "table" then return false, "core (event manager) is not available: set se.core = core from a script that has it" end
	if not se._emperor_listener then
		se._emperor_listener = true
		core:add_listener("se_emperor_policy", "FactionTurnStart", true, function(context)
			local p = se._emperor_policy
			if not p then return end
			local f = context:faction()
			local name = str(f:name())
			local info = se.query.faction(name)
			if not info or info.is_dead then return end
			for _, b in ipairs(p.banned or {}) do
				if b == name and info.level == (info.max_level or 0) - 1 then
					cm:modify_faction(name):lock_progression_level_changes()
					log("emperor_policy: locked " .. name .. " below its top level")
				end
			end
			for _, k in ipairs(p.forced or {}) do
				if k == name and not info.is_world_leader and se.available("se_faction_progression_set") then
					local used, total = 0, 3
					if info.leaders then used, total = num(info.leaders:match("^(%d+)")) or 0, num(info.leaders:match("/(%d+)")) or 3 end
					if used < total then
						local ok, msg = se_faction_progression_set(f, num(info.max_level))
						log("emperor_policy: promote " .. name .. " -> " .. str(ok) .. " : " .. str(msg))
					end
				end
			end
		end, true)
	end
	return true, policy and ("policy saved: " .. enc) or "policy cleared"
end

-- se.load_emperor_policy() : restore a saved policy (call from a LoadingGame / first-tick hook)
function se.load_emperor_policy()
	local cm, e0 = cm_()
	if not cm then return false, e0 end
	local ok, enc = pcall(function() return cm:load_named_value("se_emperor_policy", "") end)
	if not ok or type(enc) ~= "string" or enc == "" then return false, "no saved policy" end
	local forced, banned = {}, {}
	local a, b = enc:match("^(.-)|(.*)$")
	for k in (a or ""):gmatch("[^,]+") do forced[#forced + 1] = k end
	for k in (b or ""):gmatch("[^,]+") do banned[#banned + 1] = k end
	return se.modify.emperor_policy({ forced = forced, banned = banned })
end

----------------------------------------------------------------------------------------------
-- faction potential (AI handicap rating) and campaign AI personality
----------------------------------------------------------------------------------------------

-- se.query.faction_potential(key) -> { value, base, bonus, roll } | nil, message
function se.query.faction_potential(key)
	local okn, err = need("se_faction_potential_get")
	if not okn then return nil, err end
	local f, e1 = se.faction(key)
	if not f then return nil, e1 end
	local v, base, bonus, roll = se_faction_potential_get(f)
	if v == nil then return nil, str(base) end
	return { value = num(v), base = num(base), bonus = num(bonus), roll = num(roll) }
end

-- se.modify.faction_potential(key, value) : set an AI faction's potential (-100..150) and
-- re-apply its handicap effects through the engine.
function se.modify.faction_potential(key, value)
	local okn, err = need("se_faction_potential_set")
	if not okn then return false, err end
	return on_model("faction_potential(" .. str(key) .. ", " .. str(value) .. ")", function()
		local f, e1 = se.faction(key)
		if not f then return false, e1 end
		return se_faction_potential_set(f, num(value))
	end)
end

local function campaign_ai()
	local cm, err = cm_()
	if not cm then return nil, err end
	local ok, ai = pcall(function() return cm:modify_campaign_ai() end)
	if not ok or is_null(ai) then return nil, "cm:modify_campaign_ai() is not available (needs the model thread)" end
	return ai
end

-- se.query.cai_personality(faction_key) -> { key, record_key } | nil, message
-- (must run where cm:can_modify() is true, or through se.on_model; the CAI manager comes from
--  cm:modify_campaign_ai())
function se.query.cai_personality(faction_key)
	local okn, err = need("se_cai_personality_get")
	if not okn then return nil, err end
	local ai, e1 = campaign_ai()
	if not ai then return nil, e1 end
	local k, rk = se_cai_personality_get(ai, faction_key)
	if k == nil then return nil, str(rk) end
	return { key = str(k), record_key = str(rk) }
end

-- se.modify.cai_personality(faction_key, personality_key) : swap an AI faction's personality to
-- any cai_personalities key (e.g. 3k_cai_personality_dong_zhuo_late_hard).
function se.modify.cai_personality(faction_key, personality_key)
	local okn, err = need("se_cai_personality_set")
	if not okn then return false, err end
	if type(personality_key) ~= "string" or personality_key == "" then return false, "personality_key must be a non-empty string" end
	return on_model("cai_personality(" .. str(faction_key) .. ", " .. personality_key .. ")", function()
		local ai, e1 = campaign_ai()
		if not ai then return false, e1 end
		local f, e2 = se.faction(faction_key)
		if not f then return false, e2 end
		return se_cai_personality_set(ai, f, faction_key, personality_key)
	end)
end

----------------------------------------------------------------------------------------------
-- character experience and assignments (read side; phases 4 and 5)
----------------------------------------------------------------------------------------------

-- se.query.character_xp(cqi) -> { experience, rank, engine_xp, engine_rank, max_rank, skill_points, dump }
function se.query.character_xp(cqi)
	local q, err = se.character(cqi)
	if not q then return nil, err end
	local t = { experience = num(q:current_experience()), rank = num(q:rank()) }
	if se.available("se_char_rank_get") then
		local xp, rank, mx, sp = se_char_rank_get(q, num(cqi))
		if xp ~= nil then t.engine_xp, t.engine_rank, t.max_rank, t.skill_points = num(xp), num(rank), num(mx), num(sp) end
	elseif se.available("se_char_xp_get") then
		local xp, dump = se_char_xp_get(q, num(cqi))
		t.engine_xp, t.dump = num(xp), str(dump)
	end
	return t
end

-- se.modify.character_add_xp(cqi, n, scaled) : add exactly n experience (rank-ups processed by
-- the engine's own loop). scaled = true uses the engine path instead, which multiplies n by the
-- character's and faction's experience-gain modifiers.
function se.modify.character_add_xp(cqi, n, scaled)
	local okn, err = need("se_char_xp_add")
	if not okn then return false, err end
	if num(n) == nil or num(n) <= 0 then return false, "n must be a positive number" end
	return on_model("character_add_xp(" .. str(cqi) .. ", " .. str(n) .. (scaled and ", scaled" or ", raw") .. ")", function()
		local q, e1 = se.character(cqi)
		if not q then return false, e1 end
		return se_char_xp_add(q, num(cqi), num(n), not scaled)
	end)
end

-- se.query.skill_points(cqi) -> unspent skill points | nil, message
function se.query.skill_points(cqi)
	local t, err = se.query.character_xp(cqi)
	if not t then return nil, err end
	if t.skill_points == nil then return nil, "native se_char_rank_get is not available" end
	return t.skill_points
end

-- se.query.skill_tree(cqi) -> { points, rows, nodes = { {row, col, level, max, can_add, locked,
--   node, skill}, ... } } | nil, message. Read-only. row/col address the node in the engine's
--   tree; level = points in the node (0 = not taken); can_add = the engine's own check (level
--   below max, not locked, unlock requirements met), independent of the unspent points.
function se.query.skill_tree(cqi)
	local okn, err = need("se_skill_nodes")
	if not okn then return nil, err end
	local q, e1 = se.character(cqi)
	if not q then return nil, e1 end
	return se_skill_nodes(q, num(cqi))
end

-- se.modify.skill_add(cqi, row, col) : spend one unspent skill point on the node at (row, col)
--   of se.query.skill_tree(cqi) and commit it, the same engine routines as the character
--   screen's select + confirm. Refused without an unspent point or when node.can_add is false.
function se.modify.skill_add(cqi, row, col)
	local okn, err = need("se_skill_add")
	if not okn then return false, err end
	return on_model("skill_add(" .. str(cqi) .. ", " .. str(row) .. ", " .. str(col) .. ")", function()
		local q, e1 = se.character(cqi)
		if not q then return false, e1 end
		return se_skill_add(q, num(cqi), num(row), num(col))
	end)
end

-- se.modify.skill_points(cqi, n) : set the unspent skill-point counter (0..100)
function se.modify.skill_points(cqi, n)
	local okn, err = need("se_skill_points_set")
	if not okn then return false, err end
	return on_model("skill_points(" .. str(cqi) .. ", " .. str(n) .. ")", function()
		local q, e1 = se.character(cqi)
		if not q then return false, e1 end
		return se_skill_points_set(q, num(cqi), num(n))
	end)
end

-- se.query.faction_effect_value(faction_key, effect_id) -> integer value of an effect id
-- se.query.faction_xp_gain_percent(faction_key) -> the faction's character experience gain %
se.EFFECT_CHARACTER_XP_GAIN = 385   -- 0x181
function se.query.faction_effect_value(faction_key, effect_id)
	local okn, err = need("se_faction_effect_value")
	if not okn then return nil, err end
	local f, e1 = se.faction(faction_key)
	if not f then return nil, e1 end
	local v, e2 = se_faction_effect_value(f, num(effect_id))
	if v == nil then return nil, str(e2) end
	return num(v)
end
function se.query.faction_xp_gain_percent(faction_key)
	return se.query.faction_effect_value(faction_key, se.EFFECT_CHARACTER_XP_GAIN)
end

-- se.query.assignment(cqi, dump) -> { key, state, rounds, idle, province, state_index, transition_round [, engine] } | nil, message
--   province = key of the province the assignment is performed in ("" for idle assignments)
function se.query.assignment(cqi, opts_dump)
	local q, err = se.character(cqi)
	if not q then return nil, err end
	local oka, a = pcall(function() return q:active_assignment() end)
	if not oka or is_null(a) then return nil, "character has no active assignment" end
	local function g(name)
		local ok, v = pcall(function() return a[name](a) end)
		if ok then return v end
		return nil
	end
	local t = { key = str(g("assignment_record_key")), state = str(g("assignment_state")), rounds = num(g("rounds_until_state_transition")), idle = g("is_idle_assignment") == true }
	if se.available("se_assignment_info") then
		local k, prov, st, rnd = se_assignment_info(q, num(cqi))
		if k ~= nil then t.province = str(prov); t.state_index = num(st); t.transition_round = num(rnd) end
	end
	if opts_dump == true and se.available("se_assignment_dump") then t.engine = str(se_assignment_dump(q, num(cqi))) end
	return t
end

----------------------------------------------------------------------------------------------
-- main-menu build number (CcoGameCore.BuildNumber / BuildNumberShort / IsBuildModified)
----------------------------------------------------------------------------------------------

-- se.query.build_number() -> { build, short, modified } | nil, message
function se.query.build_number()
	local okn, err = need("se_build_info_get")
	if not okn then return nil, err end
	local b, sh, m = se_build_info_get()
	if b == nil then return nil, str(sh) end
	return { build = str(b), short = str(sh), modified = m == true }
end

-- se.modify.build_number(build, short, modified) : replace the menu build strings ("" or nil
-- keeps one), and optionally the "build modified" flag. Works in any Lua state (no model needed).
function se.modify.build_number(build, short, modified)
	local okn, err = need("se_build_info_set")
	if not okn then return false, err end
	return se_build_info_set(build or "", short or "", modified)
end

----------------------------------------------------------------------------------------------
-- region slots and buildings
----------------------------------------------------------------------------------------------

function se.region(key)
	local cm, err = cm_()
	if not cm then return nil, err end
	if type(key) ~= "string" or key == "" then return nil, "region key must be a non-empty string" end
	local ok, r = pcall(function() return cm:query_region(key) end)
	if not ok or is_null(r) then return nil, "no region '" .. key .. "'" end
	return r
end

-- Ordered slot entries {index, slot, name, type} of a region (index = position in slot_list()).
local function region_slots(r)
	local ok, list = pcall(function() return r:slot_list() end)
	if not ok then return nil, "slot_list failed" end
	local out = {}
	for i = 0, list:num_items() - 1 do
		local sl = list:item_at(i)
		local okn, name = pcall(function() return sl:name() end)
		local okt, ty = pcall(function() return sl:type() end)
		out[#out + 1] = { index = i, slot = sl, name = okn and str(name) or "", type = okt and str(ty) or "" }
	end
	return out
end

local function slot_of(region_key, index)
	local r, err = se.region(region_key)
	if not r then return nil, err end
	local list, e2 = region_slots(r)
	if not list then return nil, e2 end
	for _, e in ipairs(list) do if e.index == num(index) then return e, r end end
	return nil, "region " .. region_key .. " has no slot index " .. str(index)
end

-- se.query.region_slots(region_key) -> { {index, name, type, has_building, building, chain, health, engine}, ... }
function se.query.region_slots(region_key)
	local r, err = se.region(region_key)
	if not r then return nil, err end
	local list, e2 = region_slots(r)
	if not list then return nil, e2 end
	local out = {}
	for _, e in ipairs(list) do
		local row = { index = e.index, name = e.name, type = e.type }
		local okh, has = pcall(function() return e.slot:has_building() end)
		row.has_building = okh and has == true
		if row.has_building then
			local okb, b = pcall(function() return e.slot:building() end)
			if okb and not is_null(b) then
				row.building = str(b:name()); row.chain = str(b:chain()); row.health = num(b:percent_health())
			end
		end
		if se.available("se_slot_info") then
			local hb, key, health, can, info = se_slot_info(e.slot)
			if hb ~= nil then row.engine_key, row.engine_health, row.can_damage, row.engine = str(key), num(health), can == true, str(info)
			else row.engine = str(key) end
		end
		out[#out + 1] = row
	end
	return out
end

-- se.query.building_candidates(region_key, slot_index, opts) -> { level_key, ... }
--   Default lists every level the slot's current chain set offers, blocked ones included.
--   opts.only_valid = true: only what the UI would let you build right now (empty as soon as
--   the slot has a blocking reason). opts.all_chains = true: every chain the slot can hold.
function se.query.building_candidates(region_key, slot_index, opts)
	opts = opts or {}
	local okn, err = need("se_slot_candidates")
	if not okn then return nil, err end
	local e, e2 = slot_of(region_key, slot_index)
	if not e then return nil, e2 end
	local s, e3 = se_slot_candidates(e.slot, opts.only_valid == true, opts.all_chains == true)
	if s == nil or s == "" then return {}, e3 end
	local out = {}
	for k in str(s):gmatch("[^,]+") do out[#out + 1] = k end
	return out
end

-- se.query.building_options(region_key, slot_index, opts) -> { building, constructing =
--   {key, turns, elapsed} | nil, options = { {key, cost, turns, reasons, upgrade}, ... } }
--   Read-only. Every option the slot offers, blocked ones included with their reason bits:
--   reasons == 0 = the UI would allow it now; 0x10000 = province construction limit reached;
--   0x2000 / 0x4000 = cannot afford. opts.all_chains = true: every chain the slot can hold
--   (default: upgrades / conversions of the current building; an empty slot lists everything).
function se.query.building_options(region_key, slot_index, opts)
	opts = opts or {}
	local okn, err = need("se_slot_options")
	if not okn then return nil, err end
	local e, e2 = slot_of(region_key, slot_index)
	if not e then return nil, e2 end
	return se_slot_options(e.slot, opts.all_chains == true)
end

local function slot_action(tag, native, region_key, slot_index, f)
	local okn, err = need(native)
	if not okn then return false, err end
	return on_model(tag, function()
		local e, e2 = slot_of(region_key, slot_index)
		if not e then return false, e2 end
		return f(e)
	end)
end

-- se.modify.building_damage(region_key, slot_index, percent)
function se.modify.building_damage(region_key, slot_index, percent)
	return slot_action("building_damage(" .. str(region_key) .. ", " .. str(slot_index) .. ", " .. str(percent) .. ")", "se_slot_damage", region_key, slot_index, function(e)
		return se_slot_damage(e.slot, num(percent))
	end)
end

-- se.modify.building_repair(region_key, slot_index, opts) : opts.free = true repairs directly
-- (no cost); default uses the engine's repair command (charges like the UI).
function se.modify.building_repair(region_key, slot_index, opts)
	opts = opts or {}
	return slot_action("building_repair(" .. str(region_key) .. ", " .. str(slot_index) .. ")", "se_slot_repair", region_key, slot_index, function(e)
		return se_slot_repair(e.slot, opts.free == true)
	end)
end

-- se.modify.building_destroy(region_key, slot_index)
function se.modify.building_destroy(region_key, slot_index)
	return slot_action("building_destroy(" .. str(region_key) .. ", " .. str(slot_index) .. ")", "se_slot_destroy", region_key, slot_index, function(e)
		return se_slot_destroy(e.slot)
	end)
end

-- se.modify.building_construct(region_key, slot_index, level_key, opts)
--   Starts construction of level_key in the slot (upgrade / conversion = the target level key).
--   opts.force (default true): ignore the engine's blocking reasons (cost, requirements, siege);
--     force = false behaves like the UI button.
--   opts.any_chain (default true): when the key is not an upgrade of the current building, look
--     it up among every chain the slot can hold.
--   opts.free = true: no cost (anything the treasury still lost is refunded).
--   opts.turns = n: construction time in turns; opts.complete = true is turns = 1.
--   opts.pay_to_complete = true: also issue the engine's pay-to-complete-next-turn command.
function se.modify.building_construct(region_key, slot_index, level_key, opts)
	opts = opts or {}
	local okn, err = need("se_slot_construct")
	if not okn then return false, err end
	if type(level_key) ~= "string" or level_key == "" then return false, "level_key must be a non-empty string" end
	local cm, e0 = cm_()
	if not cm then return false, e0 end
	return on_model("building_construct(" .. str(region_key) .. ", " .. str(slot_index) .. ", " .. level_key .. ")", function()
		local e, e2 = slot_of(region_key, slot_index)
		if not e then return false, e2 end
		local okf, f = pcall(function() return e.slot:faction() end)
		if not okf or is_null(f) then return false, "slot has no owning faction" end
		local fkey = str(f:name())
		local before = num(f:treasury()) or 0
		local force = opts.force ~= false
		local turns = num(opts.turns) or (opts.complete and 1) or 0
		local ok, msg = se_slot_construct(e.slot, f, level_key, force, false, opts.free == true, turns)
		if not ok and opts.any_chain ~= false then
			ok, msg = se_slot_construct(e.slot, f, level_key, force, true, opts.free == true, turns)
		end
		if not ok then return false, msg end
		local report = { str(msg) }
		if opts.pay_to_complete then
			local ok2, msg2 = se_slot_pay_to_complete(e.slot)
			report[#report + 1] = "pay_to_complete -> " .. str(ok2) .. " : " .. str(msg2)
		end
		if opts.free then
			local after = num(cm:query_faction(fkey):treasury()) or before
			local spent = before - after
			if spent > 0 then
				cm:modify_faction(fkey):increase_treasury(spent)
				report[#report + 1] = "refunded " .. spent
			else
				report[#report + 1] = "nothing to refund (treasury " .. before .. " -> " .. after .. ")"
			end
		end
		return true, table.concat(report, "; ")
	end)
end

----------------------------------------------------------------------------------------------
-- alliances / coalitions
----------------------------------------------------------------------------------------------

local function alliance_by_cqi(cqi)
	local cm, err = cm_()
	if not cm then return nil, err end
	local ok, list = pcall(function() return cm:query_model():world():alliance_list() end)
	if not ok then return nil, "alliance_list failed" end
	for i = 0, list:num_items() - 1 do
		local a = list:item_at(i)
		local okc, c = pcall(function() return a:cqi() end)
		if okc and num(c) == num(cqi) then return a end
	end
	return nil, "no alliance with cqi " .. str(cqi)
end

-- se.query.alliances() -> { {cqi, name, members = {faction keys}, engine}, ... }
function se.query.alliances()
	local cm, err = cm_()
	if not cm then return nil, err end
	local ok, list = pcall(function() return cm:query_model():world():alliance_list() end)
	if not ok then return nil, "alliance_list failed" end
	local out = {}
	for i = 0, list:num_items() - 1 do
		local a = list:item_at(i)
		local row = { members = {} }
		local okc, c = pcall(function() return a:cqi() end)
		row.cqi = okc and num(c) or nil
		local okm, m = pcall(function() return a:members() end)
		if okm and type(m) == "userdata" then
			for j = 0, m:num_items() - 1 do row.members[#row.members + 1] = str(m:item_at(j):name()) end
		end
		if row.cqi and se.available("se_alliance_info") then
			local name, info = se_alliance_info(a, row.cqi)
			if name ~= nil then row.name, row.engine = str(name), str(info) else row.engine = str(info) end
		end
		out[#out + 1] = row
	end
	return out
end

-- se.modify.alliance_name(cqi, text, mode) : rename an alliance/coalition (mode "inline" default,
-- or "pointer"); check persistence with a save/load.
function se.modify.alliance_name(cqi, text, mode)
	local okn, err = need("se_alliance_name_set")
	if not okn then return false, err end
	if type(text) ~= "string" or text == "" then return false, "text must be a non-empty string" end
	return on_model("alliance_name(" .. str(cqi) .. ", " .. text .. ")", function()
		local a, e1 = alliance_by_cqi(cqi)
		if not a then return false, e1 end
		return se_alliance_name_set(a, num(cqi), text, mode or "inline")
	end)
end

----------------------------------------------------------------------------------------------
-- effect bundles
----------------------------------------------------------------------------------------------

-- se.query.effect_bundle(bundle_key [, faction_key]) -> { count, dump } | nil, message
-- (faction_key only provides the model; defaults to the local player's faction)
function se.query.effect_bundle(bundle_key, faction_key)
	local okn, err = need("se_effect_bundle_info")
	if not okn then return nil, err end
	local cm, e0 = cm_()
	if not cm then return nil, e0 end
	faction_key = faction_key or cm:get_local_faction()
	local f, e1 = se.faction(faction_key)
	if not f then return nil, e1 end
	local count, dump = se_effect_bundle_info(f, bundle_key)
	if count == nil then return nil, str(dump) end
	return { count = num(count), dump = str(dump) }
end

-- effects = { {effect = "effect_key", scope = "campaign_effect_scope_key", value = n}, ... }
local function effect_spec(effects)
	if type(effects) ~= "table" or #effects == 0 then return nil, "effects must be a non-empty list of {effect=, scope=, value=}" end
	local parts = {}
	for i, row in ipairs(effects) do
		if type(row) ~= "table" or type(row.effect) ~= "string" or type(row.scope) ~= "string" or not num(row.value) then
			return nil, "effects[" .. i .. "] needs effect (string), scope (string) and value (number)"
		end
		parts[#parts + 1] = row.effect .. "|" .. row.scope .. "|" .. string.format("%.4f", num(row.value))
	end
	return table.concat(parts, ";")
end

local function bundle_call(tag, native, faction_key, f)
	local okn, err = need(native)
	if not okn then return false, err end
	local cm, e0 = cm_()
	if not cm then return false, e0 end
	faction_key = faction_key or cm:get_local_faction()
	return on_model(tag, function()
		local fac, e1 = se.faction(faction_key)
		if not fac then return false, e1 end
		return f(fac)
	end)
end

-- se.modify.effect_bundle_define(bundle_key, effects [, faction_key])
--   Replaces the effect list of an EXISTING effect_bundles record for this game session. Every
--   later stock apply_effect_bundle(bundle_key, ...) on any holder (faction, character, region,
--   force, ...) carries the new effects; bundles applied earlier keep what they had until they
--   are removed and applied again. Not saved: define again after every load (first tick) and
--   re-apply. faction_key only provides the model.
function se.modify.effect_bundle_define(bundle_key, effects, faction_key)
	local spec, e0 = effect_spec(effects)
	if not spec then return false, e0 end
	return bundle_call("effect_bundle_define(" .. str(bundle_key) .. ")", "se_effect_bundle_define", faction_key, function(fac)
		return se_effect_bundle_define(fac, bundle_key, spec)
	end)
end

-- se.modify.effect_bundle_restore(bundle_key [, faction_key]) : stock effect list back.
function se.modify.effect_bundle_restore(bundle_key, faction_key)
	return bundle_call("effect_bundle_restore(" .. str(bundle_key) .. ")", "se_effect_bundle_restore", faction_key, function(fac)
		return se_effect_bundle_restore(fac, bundle_key)
	end)
end

-- se.modify.effect_bundle_apply_custom(faction_key, bundle_key, effects [, turns])
--   Applies bundle_key to the faction with its own effect list (the engine's per-instance custom
--   list); the DB record and other holders of the bundle are untouched. turns 0 = permanent.
function se.modify.effect_bundle_apply_custom(faction_key, bundle_key, effects, turns)
	local spec, e0 = effect_spec(effects)
	if not spec then return false, e0 end
	return bundle_call("effect_bundle_apply_custom(" .. str(faction_key) .. ", " .. str(bundle_key) .. ")", "se_effect_bundle_apply_custom", faction_key, function(fac)
		return se_effect_bundle_apply_custom(fac, bundle_key, num(turns) or 0, spec)
	end)
end

----------------------------------------------------------------------------------------------
-- faction income lines (script-side: paid into the treasury at the faction's turn start;
-- persisted through cm:save_named_value; not shown in the engine's income breakdown)
----------------------------------------------------------------------------------------------

se._income = se._income or {}

local function income_encode()
	local parts = {}
	for fk, lines in pairs(se._income) do
		for label, amount in pairs(lines) do parts[#parts + 1] = fk .. ":" .. label .. ":" .. str(amount) end
	end
	return table.concat(parts, "|")
end

local function income_install_listener()
	if se._income_listener then return true end
	local core = se.core or G("core")
	if type(core) ~= "table" then return false, "core (event manager) is not available: set se.core = core" end
	local cm = cm_()
	core:add_listener("se_income_lines", "FactionTurnStart", true, function(context)
		local name = str(context:faction():name())
		local lines = se._income[name]
		if not lines then return end
		local total = 0
		for _, amount in pairs(lines) do total = total + amount end
		if total > 0 then cm:modify_faction(name):increase_treasury(total)
		elseif total < 0 then cm:modify_faction(name):decrease_treasury(-total) end
		log("income lines: " .. name .. " " .. str(total))
	end, true)
	se._income_listener = true
	return true
end

-- se.modify.faction_income(faction_key, amount, label) : add/replace a per-turn income line
-- (amount 0 removes it). Returns ok, message.
function se.modify.faction_income(faction_key, amount, label)
	local cm, e0 = cm_()
	if not cm then return false, e0 end
	label = label or "se_income"
	local f, e1 = se.faction(faction_key)
	if not f then return false, e1 end
	se._income[faction_key] = se._income[faction_key] or {}
	if num(amount) == nil or num(amount) == 0 then se._income[faction_key][label] = nil
	else se._income[faction_key][label] = num(amount) end
	pcall(function() cm:save_named_value("se_income_lines", income_encode()) end)
	local ok, err = income_install_listener()
	if not ok then return false, err end
	return true, faction_key .. " '" .. label .. "' = " .. str(amount) .. " per turn (applied at FactionTurnStart)"
end

-- se.query.faction_income(faction_key) -> { label = amount, ... , total = n }
function se.query.faction_income(faction_key)
	local lines = se._income[faction_key] or {}
	local out, total = {}, 0
	for label, amount in pairs(lines) do out[label] = amount; total = total + amount end
	out.total = total
	return out
end

-- se.load_income_lines() : restore saved lines (call once after a campaign load)
function se.load_income_lines()
	local cm, e0 = cm_()
	if not cm then return false, e0 end
	local ok, enc = pcall(function() return cm:load_named_value("se_income_lines", "") end)
	if not ok or type(enc) ~= "string" or enc == "" then return false, "no saved income lines" end
	se._income = {}
	for part in enc:gmatch("[^|]+") do
		local fk, label, amount = part:match("^([^:]+):([^:]+):(.+)$")
		if fk then se._income[fk] = se._income[fk] or {}; se._income[fk][label] = num(amount) end
	end
	return income_install_listener()
end

----------------------------------------------------------------------------------------------
-- diplomacy: standing and attitude events
----------------------------------------------------------------------------------------------

-- se.query.attitude(a, b) -> { standing, stock } (standing of a towards b)
function se.query.attitude(a, b)
	local fa, e1 = se.faction(a)
	if not fa then return nil, e1 end
	local fb, e2 = se.faction(b)
	if not fb then return nil, e2 end
	local t = {}
	local oks, st = pcall(function() return fa:diplomatic_standing_with(fb) end)
	t.stock = oks and num(st) or nil
	if se.available("se_attitude_get") then
		local v = se_attitude_get(fa, fb)
		t.standing = num(v)
	end
	return t
end

-- se.modify.attitude(a, b, level) : fire the engine's attitude-change event from a towards b.
-- level 1/2/3 = small/medium/large positive, -1/-2/-3 = negative (values come from the DB
-- attitude event records, so a mod sets the exact numbers there).
function se.modify.attitude(a, b, level)
	local okn, err = need("se_attitude_change")
	if not okn then return false, err end
	return on_model("attitude(" .. str(a) .. ", " .. str(b) .. ", " .. str(level) .. ")", function()
		local fa, e1 = se.faction(a)
		if not fa then return false, e1 end
		local fb, e2 = se.faction(b)
		if not fb then return false, e2 end
		return se_attitude_change(fa, fb, num(level))
	end)
end

-- se.query.horde_income_hook() -> { enabled = boolean, category = 0..3, category_name = string }
--   | nil, message
--   Whether the DLL's horde income hook is actually installed in this process (cfg
--   horde_income=1 and the detour went in) and which income category receives the gdp_abs
--   total: 0 TAXES, 1 MINING (unused in 3K, the own-line route), 2 TRADE, 3 MILITARY_FORCE.
--   Read-only, no model access, safe anywhere (also in multiplayer).
local HORDE_CATEGORIES = { [0] = "TAXES", [1] = "MINING", [2] = "TRADE", [3] = "MILITARY_FORCE" }
function se.query.horde_income_hook()
	local okn, err = need("se_horde_income_hook")
	if not okn then return nil, err end
	local enabled, category = se_horde_income_hook()
	category = num(category) or 0
	return { enabled = enabled == true, category = category, category_name = HORDE_CATEGORIES[category] or "?" }
end

-- se.query.faction_force_gdp(faction_key) -> { total = n, entries = "..." } | nil, message
--   Sum of the gdp_abs ("region_gdp") values that effects put on the FACTION itself (scopes
--   force_to_faction_own*, faction_to_faction_own) or on its MILITARY FORCES (force_to_force_own,
--   what horde building bundles use). With horde_income=1 in script_extender.cfg the DLL adds
--   this total to the faction's region income 1:1, so factions without regions earn from them.
function se.query.faction_force_gdp(faction_key)
	local okn, err = need("se_faction_gdp_bonus")
	if not okn then return nil, err end
	local f, e1 = se.faction(faction_key)
	if not f then return nil, e1 end
	local total, rows = se_faction_gdp_bonus(f)
	if total == nil then return nil, str(rows) end
	return { total = num(total), entries = str(rows) }
end

----------------------------------------------------------------------------------------------
-- auto-resolve: tunables, prediction, per-battle plan and the script handler (0.24)
----------------------------------------------------------------------------------------------

se.autoresolve = se.autoresolve or {}
se._ar_vars = se._ar_vars or {}

local function parse_kv(s)
	local t = {}
	for k, v in str(s):gmatch("([%w_]+)=([^;]*)") do
		local n = tonumber(v)
		if n ~= nil then t[k] = n else t[k] = v end
	end
	return t
end

local function local_faction()
	local cm, err = cm_()
	if not cm then return nil, err end
	local ok, key = pcall(function() return cm:get_local_faction() end)
	if not ok or type(key) ~= "string" then return nil, "no local faction" end
	local f, e1 = se.faction(key)
	if not f then return nil, e1 end
	return f, key
end

-- se.query.autoresolver_variable(key) -> number | nil, message   (keys "autoresolver_*")
function se.query.autoresolver_variable(key)
	local okn, err = need("se_ar_variable_get")
	if not okn then return nil, err end
	local f, e1 = local_faction()
	if not f then return nil, e1 end
	local v, msg = se_ar_variable_get(f, str(key))
	if v == nil then return nil, str(msg) end
	return num(v)
end

-- se.query.autoresolver_variables() -> { key = value, ... } for every autoresolver_* key
function se.query.autoresolver_variables()
	local okn, err = need("se_ar_variable_list")
	if not okn then return nil, err end
	local f, e1 = local_faction()
	if not f then return nil, e1 end
	local s, msg = se_ar_variable_list(f)
	if s == nil then return nil, str(msg) end
	return parse_kv(s)
end

local function ar_vars_listener()
	if se._ar_vars_listener then return end
	local core = se.core or G("core")
	if type(core) ~= "table" then return end
	se._ar_vars_listener = true
	-- The engine rebuilds the variable array when its per-round overrides change; put the
	-- script's values back at the start of every turn of the local player.
	core:add_listener("se_ar_vars", "FactionTurnStart", true, function(context)
		-- every faction's turn start, on every machine alike (multiplayer-safe)
		local okf, fkey = pcall(function() return context:faction():name() end)
		if not okf then return end
		local f = se.faction(fkey)
		if not f then return end
		for k, v in pairs(se._ar_vars) do se_ar_variable_set(f, k, v) end
	end, true)
end

-- se.modify.autoresolver_variable(key, value): retune one of the simulator's constants for this
-- session (not saved: call it again after a load). Only "autoresolver_*" keys.
function se.modify.autoresolver_variable(key, value)
	local okn, err = need("se_ar_variable_set")
	if not okn then return false, err end
	if type(key) ~= "string" or not num(value) then return false, "usage: autoresolver_variable(key, number)" end
	return on_model("autoresolver_variable(" .. key .. ", " .. str(value) .. ")", function()
		local f, e1 = local_faction()
		if not f then return false, e1 end
		local ok, msg = se_ar_variable_set(f, key, num(value))
		if ok then se._ar_vars[key] = num(value); ar_vars_listener() end
		return ok, msg
	end)
end

-- se.modify.autoresolver_variables_reset(): every variable back to what the engine had.
function se.modify.autoresolver_variables_reset()
	local okn, err = need("se_ar_variables_reset")
	if not okn then return false, err end
	return on_model("autoresolver_variables_reset()", function()
		local f, e1 = local_faction()
		if not f then return false, e1 end
		se._ar_vars = {}
		return se_ar_variables_reset(f)
	end)
end

-- se.query.autoresolve_prediction() -> { available, attacker_prediction, defender_prediction,
--   <side>_casualties_percent, <side>_strength_share, <side>_men_before / _men_after / _men_lost,
--   night, result_index, results } | nil, message
-- The engine's own prediction for the pending battle (what the pre-battle panel shows).
function se.query.autoresolve_prediction()
	local okn, err = need("se_ar_prediction")
	if not okn then return nil, err end
	local f, e1 = local_faction()
	if not f then return nil, e1 end
	local ok, s = se_ar_prediction(f)
	if not ok then return nil, str(s) end
	local t = parse_kv(s)
	t.available = t.available == 1
	return t
end

local function try(obj, method, arg)
	if obj == nil then return nil end
	local ok, v = pcall(function()
		if arg ~= nil then return obj[method](obj, arg) end
		return obj[method](obj)
	end)
	if ok then return v end
	return nil
end

local function side_of(general, secondary, local_key)
	local side = { forces = {}, characters = {} }
	local function add(ch)
		if ch == nil or is_null(ch) then return end
		local fac = try(ch, "faction")
		local row = { cqi = num(try(ch, "cqi") or try(ch, "command_queue_index")), faction = str(try(fac, "name")),
			template = str(try(ch, "generation_template_key")), rank = num(try(ch, "rank")) }
		side.characters[#side.characters + 1] = row
		if side.faction == nil then side.faction = row.faction; side.is_human = try(fac, "is_human") == true end
		if row.faction == local_key then side.is_local_player = true end
		if try(ch, "has_military_force") == true then
			local mf = try(ch, "military_force")
			local units = try(mf, "unit_list")
			side.forces[#side.forces + 1] = { cqi = num(try(mf, "command_queue_index")), general_cqi = row.cqi, units = num(try(units, "num_items")) }
		end
	end
	add(general)
	if secondary ~= nil then
		for i = 0, (num(try(secondary, "num_items")) or 0) - 1 do add(try(secondary, "item_at", i)) end
	end
	return side
end

-- se.query.pending_battle() -> context table | nil, message
--   { active, battle_type, is_siege, is_ambush, is_night, human_involved,
--     attacker = { faction, is_human, is_local_player, strength, forces = {{cqi, general_cqi, units}},
--                  characters = {{cqi, faction, template, rank}} }, defender = { ... },
--     prediction = se.query.autoresolve_prediction() }
function se.query.pending_battle()
	local cm, err = cm_()
	if not cm then return nil, err end
	local ok, pb = pcall(function() return cm:query_model():pending_battle() end)
	if not ok or pb == nil or is_null(pb) then return nil, "no pending battle interface" end
	local okl, local_key = pcall(function() return cm:get_local_faction() end)
	local ctx = { active = try(pb, "is_active") == true, battle_type = str(try(pb, "battle_type")),
		is_siege = try(pb, "seige_battle") == true, is_ambush = try(pb, "ambush_battle") == true,
		is_night = try(pb, "night_battle") == true, human_involved = try(pb, "human_involved") == true }
	ctx.attacker = side_of(try(pb, "has_attacker") == true and try(pb, "attacker") or nil, try(pb, "secondary_attackers"), okl and local_key or nil)
	ctx.defender = side_of(try(pb, "has_defender") == true and try(pb, "defender") or nil, try(pb, "secondary_defenders"), okl and local_key or nil)
	ctx.attacker.strength = num(try(pb, "attacker_strength"))
	ctx.defender.strength = num(try(pb, "defender_strength"))
	ctx.local_player_involved = ctx.attacker.is_local_player == true or ctx.defender.is_local_player == true
	-- deterministic seed for chance-based rules: identical on every machine of a multiplayer game
	local okt, turn = pcall(function() return cm:query_model():turn_number() end)
	local af, df = ctx.attacker.forces[1], ctx.defender.forces[1]
	ctx.seed = ((okt and num(turn) or 0) * 7919 + (af and af.cqi or 0) * 104729 + (df and df.cqi or 0) * 1299709) % 16777213
	ctx.prediction = se.query.autoresolve_prediction()
	return ctx
end

local function clamp(v, lo, hi, default)
	v = num(v)
	if v == nil then return default end
	if v < lo then return lo elseif v > hi then return hi end
	return v
end

local FATES = { kill = true, wound = true, spare = true, flee = true }

-- Land unit key of a character's own (hero) unit: needed to create a duel the engine did not
-- roll. Resolved only when the character's force has exactly one hero unit; otherwise the plan
-- has to give a_key / b_key itself.
local function hero_unit_key(cqi)
	local ch = se.character(cqi)
	if not ch or try(ch, "has_military_force") ~= true then return nil end
	local units = try(try(ch, "military_force"), "unit_list")
	local found, n = nil, 0
	for i = 0, (num(try(units, "num_items")) or 0) - 1 do
		local key = try(try(units, "item_at", i), "unit_key")
		if type(key) == "string" and key:find("_hero_") then found = key; n = n + 1 end
	end
	if n == 1 then return found end
	return nil
end

-- plan -> "k=v;..." for the DLL. Values are clamped here (bias 0.1-10, scales and chances 0-1).
local function encode_plan(plan, ctx)
	if type(plan) ~= "table" then return nil, "plan must be a table" end
	local parts = { "v=1", "seed=" .. str(ctx and ctx.seed or 0) }
	local function put(k, v) if v ~= nil then parts[#parts + 1] = k .. "=" .. str(v) end end
	local af = ctx and ctx.attacker and ctx.attacker.forces[1]
	local df = ctx and ctx.defender and ctx.defender.forces[1]
	put("att_force", af and af.cqi); put("def_force", df and df.cqi)
	if type(plan.bias) == "table" then
		put("bias_att", clamp(plan.bias.attacker, 0.1, 10, nil)); put("bias_def", clamp(plan.bias.defender, 0.1, 10, nil))
	end
	if plan.winner ~= nil then
		if plan.winner ~= "attacker" and plan.winner ~= "defender" then return nil, "plan.winner must be 'attacker', 'defender' or nil" end
		put("winner", plan.winner)
	end
	if type(plan.casualties) == "table" then
		for side, tag in pairs({ attacker = "att", defender = "def" }) do
			local c = plan.casualties[side]
			if type(c) == "table" then
				put("cas_" .. tag .. "_scale", clamp(c.scale, 0, 10, nil)); put("cas_" .. tag .. "_max", clamp(c.max, 0, 1, nil))
			end
		end
	end
	if type(plan.duels) == "table" then
		put("duel_max", clamp(plan.duels.max, 0, 16, nil))
		if plan.duels.default ~= nil then
			if plan.duels.default ~= "vanilla" and plan.duels.default ~= "none" then return nil, "plan.duels.default must be 'vanilla' or 'none'" end
			put("duel_default", plan.duels.default)
		end
		local rows = {}
		for i, d in ipairs(plan.duels.pairs or {}) do
			if type(d) ~= "table" or not num(d.a) or not num(d.b) then return nil, "plan.duels.pairs[" .. i .. "] needs character cqis a and b" end
			if d.fate ~= nil and not FATES[d.fate] then return nil, "plan.duels.pairs[" .. i .. "].fate must be kill, wound, spare or flee" end
			local a_key = type(d.a_key) == "string" and d.a_key or hero_unit_key(num(d.a)) or ""
			local b_key = type(d.b_key) == "string" and d.b_key or hero_unit_key(num(d.b)) or ""
			rows[#rows + 1] = table.concat({ str(num(d.a)), str(num(d.b)), d.happen == false and "0" or "1",
				str(clamp(d.win_chance, 0, 1, -1)), str(num(d.winner) or -1), d.fate or "-", a_key, b_key }, ",")
		end
		if #rows > 0 then put("duels", table.concat(rows, "|")) end
	end
	return table.concat(parts, ";")
end

-- se.modify.autoresolve_plan(plan [, ctx]): store the plan for the current pending battle.
-- The engine hook applies plan.winner and plan.casualties (0.26.2+) and plan.duels (0.27+):
--   duels.default = "none" removes every duel without a rule, duels.max trims the list,
--   pairs[i] = { a = cqi, b = cqi, happen = false } forbids that duel,
--   { a, b, winner = cqi } or { a, b, win_chance = 0..1 (chance that a wins) } sets the winner,
--   and a pair the engine did not roll is created when both hero unit keys are known (a_key /
--   b_key, looked up automatically for forces with a single hero unit). `fate` is accepted but
--   not applied yet (the loser's wound / death is rolled by the campaign afterwards), and
--   plan.bias is stored only.
-- plan.refresh_prediction = false skips the immediate recompute of the panel prediction.
function se.modify.autoresolve_plan(plan, ctx)
	local okn, err = need("se_ar_plan_set")
	if not okn then return false, err end
	local cm, e0 = cm_()
	if not cm then return false, e0 end
	ctx = ctx or se.query.pending_battle()
	if not ctx or not ctx.human_involved then return false, "refused: no pending battle with a human player" end
	local spec, e1 = encode_plan(plan, ctx)
	if not spec then return false, e1 end
	se._ar_plan = plan
	return on_model("autoresolve_plan", function()
		local f, e2 = local_faction()
		if not f then return false, e2 end
		local ok, msg = se_ar_plan_set(f, spec)
		if ok and plan.refresh_prediction ~= false and se.available("se_ar_recompute") then
			-- run the engine's compute routine again so the pre-battle panel shows the planned result
			local ok2, msg2 = se_ar_recompute(f)
			msg = str(msg) .. "; recompute -> " .. str(ok2) .. " : " .. str(msg2)
		end
		return ok, msg
	end)
end

function se.modify.autoresolve_plan_clear()
	local okn, err = need("se_ar_plan_clear")
	if not okn then return false, err end
	se._ar_plan = nil
	return se_ar_plan_clear()
end

-- se.query.autoresolve_plan() -> plan table (as given), encoded string
function se.query.autoresolve_plan()
	local s = se.available("se_ar_plan_get") and se_ar_plan_get() or ""
	return se._ar_plan, str(s)
end

-- se.autoresolve.set_handler(function(ctx) return plan_or_nil end)
--   Called on every PendingBattle that involves a human player, on every machine, with
--   the table of se.query.pending_battle(). Return a plan table to steer that auto-resolve, or
--   nil for vanilla behaviour. The plan is dropped again on BattleCompleted.
function se.autoresolve.set_handler(fn)
	if type(fn) ~= "function" then return false, "handler must be a function" end
	se.autoresolve.handler = fn
	if se._ar_listener then return true, "handler replaced" end
	local core = se.core or G("core")
	if type(core) ~= "table" then return false, "core (event manager) is not available: set se.core = core first" end
	se._ar_listener = true
	core:add_listener("se_ar_pending", "PendingBattle", true, function()
		local h = se.autoresolve.handler
		if type(h) ~= "function" then return end
		pcall(se.modify.autoresolve_plan_clear)
		local ctx = se.query.pending_battle()
		-- human_involved is the same on every machine; is_local_player is NOT, so a handler
		-- used in multiplayer must never branch on it (use is_human / faction keys instead)
		if not ctx or not ctx.human_involved then return end
		local okh, plan = pcall(h, ctx)
		if not okh then log("autoresolve handler error: " .. str(plan)) return end
		if plan == nil then log("autoresolve handler: vanilla (nil plan)") return end
		local ok, msg = se.modify.autoresolve_plan(plan, ctx)
		log("autoresolve plan -> " .. str(ok) .. " : " .. str(msg))
	end, true)
	core:add_listener("se_ar_completed", "BattleCompleted", true, function()
		pcall(se.modify.autoresolve_plan_clear)
	end, true)
	return true, "handler installed"
end

function se.autoresolve.clear_handler()
	se.autoresolve.handler = nil
	return se.modify.autoresolve_plan_clear()
end

-- se.modify.duel_power_bonus({ [character_cqi] = bonus, ... }) (DLL 0.42.0-beta.5+)
--   Adds `bonus` to the character's auto-resolve duel power before the engine decides the duel
--   (vanilla power = main_units melee_cp + missile_cp + rank + special abilities; stronger wins,
--   a gap above autoresolver_duel_refuse_variable = refused). Entries are merged into the DLL's
--   table and stay until changed; bonus 0 removes an entry; values are clamped to +-2000. Applies
--   to every auto-resolve, AI battles included. Not saved: set it again after a load (first tick).
function se.modify.duel_power_bonus(map)
	local okn, err = need("se_duel_bonus_set")
	if not okn then return false, err end
	if type(map) ~= "table" then return false, "map must be a table { [cqi] = bonus }" end
	local rows = {}
	for cqi, bonus in pairs(map) do
		local c, b = num(cqi), num(bonus)
		if not c or not b then return false, "map entries must be numbers (got " .. str(cqi) .. " = " .. str(bonus) .. ")" end
		rows[#rows + 1] = str(c) .. ":" .. str(b)
	end
	if #rows == 0 then return true, "nothing to set" end
	local spec = table.concat(rows, "|")
	return on_model("duel_power_bonus", function() return se_duel_bonus_set(spec) end)
end

function se.modify.duel_power_bonus_clear()
	local okn, err = need("se_duel_bonus_clear")
	if not okn then return false, err end
	return on_model("duel_power_bonus_clear", function() return se_duel_bonus_clear() end)
end

-- se.query.duel_power_hook() -> { installed, entries, formula } | nil, message
--   installed = false when script_extender.cfg has duel_power_hook=0 (or autoresolve_hooks=0);
--   formula = true while se.modify.duel_power_formula is active (DLL 0.43.0+).
function se.query.duel_power_hook()
	local okn, err = need("se_duel_bonus_info")
	if not okn then return nil, err end
	local t = {}
	for k, v in str(se_duel_bonus_info()):gmatch("([%w_]+)=([^;]*)") do t[k] = tonumber(v) or v end
	t.installed = t.installed == 1
	t.formula = t.formula == 1
	return t
end

-- se.modify.duel_power_formula(f) (DLL 0.43.0+): duel power becomes a weighted sum of the
-- duelist's own stats - the numbers of the unit card, with attributes, skills, equipment and
-- effects applied - instead of the unit's fixed main_units combat potential.
--   f = { vanilla = 0,          -- weight on the vanilla power (0 = replace it)
--         abilities = 1,        -- weight on the vanilla special-ability term
--         health = 0.01,        -- per current hit point
--         health_scale = true,  -- multiply the stat and ability terms by the health fraction
--         refuse = 600,         -- optional duel refusal gap while the formula is active (the
--                               -- game's value, 150, comes back when it is cleared)
--         stats = { stat_armour = 1, stat_melee_defence = 2, stat_melee_damage_base = 0.1, ... } }
--   Stat keys are the exe's names (se.query.duel_stat_names()). The per-character bonus of
--   se.modify.duel_power_bonus still adds on top. Not saved: set it again at every first tick.
function se.modify.duel_power_formula(f)
	local okn, err = need("se_duel_formula_set")
	if not okn then return false, err end
	if type(f) ~= "table" then return false, "formula must be a table" end
	local parts = {}
	local function put(k, v)
		local n = num(v)
		if n == nil then return false end
		parts[#parts + 1] = k .. "=" .. str(n)
		return true
	end
	for _, k in ipairs({ "vanilla", "abilities", "health", "refuse" }) do
		if f[k] ~= nil and not put(k, f[k]) then return false, k .. " must be a number" end
	end
	if f.health_scale ~= nil then parts[#parts + 1] = "health_scale=" .. (f.health_scale and "1" or "0") end
	if f.stats ~= nil and type(f.stats) ~= "table" then return false, "stats must be a table { stat_name = weight }" end
	local keys = {}
	for k in pairs(f.stats or {}) do keys[#keys + 1] = k end
	table.sort(keys) -- same text on every machine
	for _, k in ipairs(keys) do
		if type(k) ~= "string" or not put(k, f.stats[k]) then return false, "stats." .. str(k) .. " must be a number" end
	end
	local spec = table.concat(parts, ";")
	return on_model("duel_power_formula", function() return se_duel_formula_set(spec) end)
end

function se.modify.duel_power_formula_clear()
	local okn, err = need("se_duel_formula_clear")
	if not okn then return false, err end
	return on_model("duel_power_formula_clear", function() return se_duel_formula_clear() end)
end

-- se.query.duel_power_formula() -> encoded formula ("" when none)
function se.query.duel_power_formula()
	local okn, err = need("se_duel_formula_get")
	if not okn then return nil, err end
	return str(se_duel_formula_get())
end

-- se.query.duel_stat_names() -> { stat_armour = 3, stat_melee_defence = 22, ... } | nil, message
function se.query.duel_stat_names()
	local okn, err = need("se_duel_stat_names")
	if not okn then return nil, err end
	local s, e = se_duel_stat_names()
	if not s then return nil, e end
	local t = {}
	for k, v in str(s):gmatch("([%w_]+)=([^;]*)") do t[k] = tonumber(v) end
	return t
end

-- se.query.duel_last(cqi) -> "vanilla 1300 -> 1450 [armour=65 ... hp=18360/18360]" | nil
--   The breakdown of the character's most recent auto-resolve duel candidacy (prediction or
--   result), for tuning a formula.
function se.query.duel_last(cqi)
	local okn, err = need("se_duel_last")
	if not okn then return nil, err end
	return se_duel_last(num(cqi))
end

----------------------------------------------------------------------------------------------
-- diagnostics
----------------------------------------------------------------------------------------------

se.profile = se.profile or {}

-- se.query.perf() -> { installed, ttl_ms, hits, misses, passed_through, entries, miss_*, stamp_failed, ai_*, perm_*, file_probes, file_probes_skipped }
--   Counters of the UI recruit-list cache (script_extender.cfg: ui_recruit_cache_ms, default
--   250, 0 = off). hits = UI queries answered from the cache, misses = UI queries the engine
--   computed, passed_through = calls from the AI and other non-UI callers (never cached).
function se.query.perf()
	local okn, err = need("se_perf_stats")
	if not okn then return nil, err end
	local t = {}
	for k, v in str(se_perf_stats()):gmatch("([%w_]+)=([^;]*)") do t[k] = tonumber(v) or v end
	t.installed = t.installed == 1
	return t
end

-- se.profile.start(seconds [, delay_seconds [, label]]) -> ok, message
--   Sampling profiler of the game process (1 ms, the six busiest threads, re-picked every
--   second). Waits `delay_seconds` (default 3) so the console can be closed, samples for
--   `seconds` (1..120), then writes profile_<label>.txt next to the DLL: per thread the functions
--   the CPU was in ("self") and the functions on the stack, as Ghidra addresses. Reads only;
--   costs a few percent of frame time while it runs. One run at a time.
function se.profile.start(seconds, delay_seconds, label)
	local okn, err = need("se_profile_start")
	if not okn then return false, err end
	return se_profile_start(num(seconds) or 20, num(delay_seconds) or 3, str(label or "run"))
end

-- se.profile.stop() -> ok, message : end the running profile now (its reports are still written).
function se.profile.stop()
	local okn, err = need("se_profile_stop")
	if not okn then return false, err end
	return se_profile_stop()
end

-- Pretty-print helper for console use: se.dump(se.query.retinue(1))
function se.dump(v, indent)
	indent = indent or ""
	if type(v) ~= "table" then return str(v) end
	local keys = {}
	for k in pairs(v) do keys[#keys + 1] = k end
	table.sort(keys, function(a, b) return str(a) < str(b) end)
	local parts = {}
	for _, k in ipairs(keys) do
		local val = v[k]
		if type(val) == "table" then
			parts[#parts + 1] = indent .. str(k) .. ":\n" .. se.dump(val, indent .. "  ")
		else
			parts[#parts + 1] = indent .. str(k) .. " = " .. str(val)
		end
	end
	return table.concat(parts, "\n")
end

----------------------------------------------------------------------------------------------
-- crash reporter (DLL 0.42+; script_extender.cfg diag_crash: 0 off, 1 on (default), 2 + trace)
----------------------------------------------------------------------------------------------

se.crash = se.crash or {}

-- se.crash.mark(text) -> ok, message
--   A breadcrumb for the DLL's crash report (se_crash.txt next to the DLL): when the game
--   faults, the report lists the last 64 SE events of all threads, marks included, and the hook
--   or native the faulting thread was in. 63 bytes of the text are kept. Put one around a
--   suspect script action: se.crash.mark("before recruit " .. key).
function se.crash.mark(text)
	local okn, err = need("se_crash_mark")
	if not okn then return false, err end
	return se_crash_mark(str(text))
end

-- se.crash.info() -> { installed, level, natives, hooks, reports, repeats, dropped, last_code, last_rva, scope_overflow } | nil, message
--   reports = crash reports written this session, repeats = faults suppressed as duplicates.
function se.crash.info()
	local okn, err = need("se_crash_info")
	if not okn then return nil, err end
	local t = {}
	for k, v in str(se_crash_info()):gmatch("([%w_]+)=([^;]*)") do t[k] = tonumber(v) or v end
	t.installed = t.installed == 1
	return t
end

-- se.crash.selftest([mode]) -> ok, message
--   Writes a test report to se_crash.txt without faulting: mode 1 (default) from the current
--   context directly, mode 2 through the exception handler (a private exception the handler
--   dismisses; pauses the game while a debugger is attached).
function se.crash.selftest(mode)
	local okn, err = need("se_crash_selftest")
	if not okn then return false, err end
	return se_crash_selftest(num(mode) or 1)
end

----------------------------------------------------------------------------------------------
-- diagnostics: which script listeners cost the time (DLL 0.35+)
----------------------------------------------------------------------------------------------

se.diag = se.diag or {}

-- se.diag.listeners_start() -> ok, message
--   Wraps the condition and the callback of every listener registered with core:add_listener
--   (existing ones and those added later) in a stopwatch. Behaviour is unchanged: same
--   arguments, same return value, errors pass through to the game's own pcall. Times are
--   inclusive (a callback that triggers other events includes their listeners). Read-only
--   towards the game; not saved; lasts until the Lua state is rebuilt (load / restart).
-- se.diag.listeners_report([top]) -> array of rows, also written to the log
--   rows sorted by total time: { event, name, calls, fired, total_ms, condition_ms, callback_ms }
-- se.diag.listeners_reset() : zero the numbers (e.g. right before End Turn)
local diag_rows, diag_on, diag_level = {}, false, 0

local function diag_row(listener)
	local key = str(listener.event) .. "|" .. str(listener.name)
	local row = diag_rows[key]
	if not row then
		row = { event = str(listener.event), name = str(listener.name), calls = 0, fired = 0, condition_us = 0, callback_us = 0 }
		diag_rows[key] = row
	end
	return row
end

local function diag_wrap(listener, push, pop)
	if type(listener) ~= "table" or listener.__se_timed then return end
	listener.__se_timed = true
	local row = diag_row(listener)
	local condition, callback = listener.condition, listener.callback
	if type(condition) == "function" then
		listener.condition = function(context)
			local level = diag_level + 1
			diag_level = level
			local d = push(level)
			local r = condition(context)
			row.condition_us = row.condition_us + pop(d)
			row.calls = row.calls + 1
			diag_level = level - 1
			return r
		end
	end
	if type(callback) == "function" then
		listener.callback = function(context)
			local level = diag_level + 1
			diag_level = level
			local d = push(level)
			local r = callback(context)
			row.callback_us = row.callback_us + pop(d)
			row.fired = row.fired + 1
			diag_level = level - 1
			return r
		end
	end
end

function se.diag.listeners_start()
	local okn, err = need("se_timer_push", "se_timer_pop")
	if not okn then return false, err end
	local core = se.core or G("core")
	if type(core) ~= "table" or type(core.event_listeners) ~= "table" then return false, "core (event manager) is not available: set se.core = core first" end
	local push, pop = G("se_timer_push"), G("se_timer_pop")
	local wrapped = 0
	for _, list in pairs(core.event_listeners) do
		if type(list) == "table" then
			for i = 1, #list do
				if type(list[i]) == "table" and not list[i].__se_timed then wrapped = wrapped + 1 end
				diag_wrap(list[i], push, pop)
			end
		end
	end
	if not diag_on then
		diag_on = true
		local add = core.add_listener
		core.add_listener = function(self, name, event, condition, callback, persistent)
			local r = add(self, name, event, condition, callback, persistent)
			local list = self.event_listeners and self.event_listeners[event]
			if type(list) == "table" and list[#list] and list[#list].name == name then diag_wrap(list[#list], push, pop) end
			return r
		end
		-- a listener that raises an error never reaches its own bookkeeping: the dispatcher
		-- (which pcalls every listener) restores the nesting level afterwards
		local dispatch = core.event_callback
		if type(dispatch) == "function" then
			core.event_callback = function(self, eventname, context)
				local level = diag_level
				local r = dispatch(self, eventname, context)
				diag_level = level
				return r
			end
		end
	end
	return true, "timing " .. wrapped .. " listeners (new ones are added as they register)"
end

function se.diag.listeners_reset()
	for _, row in pairs(diag_rows) do
		row.calls, row.fired, row.condition_us, row.callback_us = 0, 0, 0, 0
	end
	return true
end

function se.diag.listeners_report(top)
	local rows, events = {}, {}
	for _, row in pairs(diag_rows) do
		local total = row.condition_us + row.callback_us
		if row.calls > 0 or row.fired > 0 then
			rows[#rows + 1] = { event = row.event, name = row.name, calls = row.calls, fired = row.fired,
				total_ms = total / 1000, condition_ms = row.condition_us / 1000, callback_ms = row.callback_us / 1000 }
			events[row.event] = (events[row.event] or 0) + total / 1000
		end
	end
	table.sort(rows, function(a, b) return a.total_ms > b.total_ms end)
	local sum = 0
	for _, r in ipairs(rows) do sum = sum + r.total_ms end
	log(string.format("listener timing: %d active listeners, %.0f ms in total (inclusive times)", #rows, sum))
	local by_event = {}
	for e, ms in pairs(events) do by_event[#by_event + 1] = { e, ms } end
	table.sort(by_event, function(a, b) return a[2] > b[2] end)
	for i = 1, math.min(#by_event, 12) do
		log(string.format("  event %-44s %9.1f ms", by_event[i][1], by_event[i][2]))
	end
	for i = 1, math.min(#rows, num(top) or 30) do
		local r = rows[i]
		log(string.format("  %9.1f ms  cond %8.1f  cb %8.1f  calls %6d fired %6d  %s / %s", r.total_ms, r.condition_ms, r.callback_ms, r.calls, r.fired, r.event, r.name))
	end
	return rows
end

-- se.diag.diplomacy_trace(on [, include_ai]) -> was_on | nil, message
--   Records every "is this treaty component valid between A and B" question the engine asks
--   (diplomacy screen, ultimatum popups, scripted deals; the campaign AI's too with include_ai)
--   together with the campaign_diplomacy_groups tree it walked. Needs diag_diplomacy=1 in
--   script_extender.cfg at injection. Read-only; switching it on clears the previous recording.
function se.diag.diplomacy_trace(on, include_ai)
	local okn, err = need("se_dip_trace")
	if not okn then return nil, err end
	return se_dip_trace(on and true or false, include_ai and true or false)
end

-- se.diag.diplomacy_mark(label) : a time marker between the recorded evaluations (a click, a step
--   of a test), so the report shows what was asked after it.
function se.diag.diplomacy_mark(label)
	if type(G("se_dip_trace_mark")) == "function" then se_dip_trace_mark(str(label)) end
end

-- se.diag.diplomacy_report() -> { path, evaluations, blocked, summary = { line, ... } } | nil, message
--   Writes dip_trace.txt next to the DLL (one entry per distinct evaluation, with the walked
--   tree: '+' passed, '-' failed) and logs the blocked ones: component | A -> B | flags | reason.
function se.diag.diplomacy_report()
	local okn, err = need("se_dip_trace_report")
	if not okn then return nil, err end
	local path, evals, blocked, summary = se_dip_trace_report()
	local t = { path = str(path), evaluations = num(evals) or 0, blocked = num(blocked) or 0, summary = {} }
	for line in str(summary):gmatch("[^\n]+") do t.summary[#t.summary + 1] = line end
	log(string.format("diplomacy trace: %d distinct evaluation(s), %d blocked -> %s", t.evaluations, t.blocked, t.path))
	for _, line in ipairs(t.summary) do log("  BLOCKED " .. line) end
	return t
end

----------------------------------------------------------------------------------------------
-- UI repairs (DLL 0.41+)
----------------------------------------------------------------------------------------------

se.ui = se.ui or {}

local FOLLOWUP_PANEL = "diplomacy_followup_negotiation_popup_panel"
local RESPOND_PANEL = "diplomacy_popup_panel"
local FOLLOWUP_BUTTONS = { button_accept = true, button_txt = true, button_reject = true }

-- Which popup holds the clicked component, and which of its buttons was hit:
--   "followup" (the player proposes: MEDIATE PEACE), "respond" (an offer the player answers:
--   the emperor holder's ultimatum), false = another panel, nil = cannot tell (no UIComponent in
--   reach: hand it over as se.UIComponent; the natives have their own guards then).
--   button = "accept" / "reject" when button_accept / button_reject is the component or a parent
--   (a click on a button's label lands on its button_txt child).
--   UIComponent is a callable table in the campaign state (0.46.0-beta.2: a table was taken for
--   "no UIComponent" before, so a click on a label could not be answered); a component that cannot
--   be converted or read means "cannot tell" (nil), never "another panel".
local function popup_of(address)
	local UIC = se.UIComponent or G("UIComponent")
	if type(UIC) ~= "function" and type(UIC) ~= "table" then return nil end
	local okc, uic = pcall(UIC, address)
	local button
	for _ = 1, 16 do
		if not okc or not uic then return nil end
		local oki, id = pcall(function() return uic:Id() end)
		if not oki or type(id) ~= "string" then return nil end
		if button == nil and id == "button_accept" then button = "accept" end
		if button == nil and id == "button_reject" then button = "reject" end
		if id == FOLLOWUP_PANEL then return "followup", button end
		if id == RESPOND_PANEL then return "respond", button end
		local okp, parent = pcall(function() return uic:Parent() end)
		if not okp or not parent then return false end
		okc, uic = pcall(UIC, parent)
	end
	return false
end

-- se.modify.followup_propose() -> ok, message
--   Runs the engine's own "propose" of the follow-up negotiation popup that is on screen (the
--   ultimatum raised when a faction you vassalise is at war: MEDIATE PEACE). The recipient then
--   accepts or refuses exactly as in the unmodified game; the command travels through the normal
--   command queue. Refused when no such popup is up or the engine just handled the click itself.
function se.modify.followup_propose()
	local okn, err = need("se_followup_propose")
	if not okn then return false, err end
	return se_followup_propose()
end

-- se.modify.followup_respond(answer) -> ok, message   (DLL 0.45.1+; answer = "accept" | "reject")
--   The other side of a follow-up: the AI's follow-up offer to the player (the emperor holder's
--   "Issue Ultimatum": peace on behalf of the Han, or war). Sends the engine's own accept / reject
--   command for the offer that is on screen, through the normal command queue. Refused when no
--   such offer is waiting or the engine just handled the click itself.
function se.modify.followup_respond(answer)
	local okn, err = need("se_followup_respond")
	if not okn then return false, err end
	if answer ~= "accept" and answer ~= "reject" then return false, "answer must be \"accept\" or \"reject\"" end
	return se_followup_respond(answer)
end

local function trace_mark(text)
	if type(G("se_dip_trace_mark")) == "function" then se_dip_trace_mark(text) end
end

-- se.ui.fix_followup_button() -> ok, message
--   On game build 1.7.2.0 the call-for-action button of that popup is lit but dead: a click never
--   reaches its ProposeDeal command. This listens for the click and proposes through
--   se.modify.followup_propose(). Since DLL 0.45.1 it also repairs the other side: ACCEPT /
--   REJECT of a follow-up offer the player answers (diplomacy_popup) go through
--   se.modify.followup_respond(). Needs se.core = core. Call it once per campaign session (first
--   tick); calling it again replaces the listener.
function se.ui.fix_followup_button()
	local okn, err = need("se_followup_propose")
	if not okn then return false, err end
	local installed, why = se_followup_propose("installed")
	if not installed then return false, why end
	local core = se.core or G("core")
	if type(core) ~= "table" or type(core.add_listener) ~= "function" then return false, "core (event manager) is not available: set se.core = core first" end
	local respond_on = type(G("se_followup_respond")) == "function" and se_followup_respond("installed") == true
	pcall(function() core:remove_listener("se_followup_button") end)
	core:add_listener("se_followup_button", "ComponentLClickUp",
		function(context) return FOLLOWUP_BUTTONS[context.string] == true end,
		function(context)
			local kind, button = popup_of(context.component)
			if kind == false then return end
			if button == nil then
				if context.string == "button_accept" then button = "accept" elseif context.string == "button_reject" then button = "reject" end
			end
			-- an offer the player answers: the native refuses unless one is waiting on screen
			if respond_on and kind ~= "followup" and button ~= nil then
				local ok, msg = se_followup_respond(button)
				trace_mark("lua click " .. str(context.string) .. " answer " .. button .. " -> " .. str(ok) .. " " .. str(msg))
				log("follow-up offer button (" .. button .. "): " .. str(ok) .. " " .. str(msg))
				if ok or kind == "respond" then return end
			end
			if kind == "respond" or button == "reject" then return end
			local ok, msg = se.modify.followup_propose()
			trace_mark("lua click " .. str(context.string) .. " -> " .. str(ok) .. " " .. str(msg))
			log("follow-up popup button: " .. str(ok) .. " " .. str(msg))
		end, true)
	return true, respond_on and "listening (propose and answer)" or "listening (propose)"
end

----------------------------------------------------------------------------------------------
-- AI recruitment: what the campaign AI budgets, what it buys, what it could have bought
-- (DLL 0.36+, read-only; notes/ai_recruitment.md)
----------------------------------------------------------------------------------------------

se.ai_recruit = se.ai_recruit or {}
-- se.ai_recruit.quality_override[unit_key] = number : replaces the game's quality for that unit
-- in every group (mod units missing from cdir_military_generator_unit_qualities, or a rebalance)
se.ai_recruit.quality_override = se.ai_recruit.quality_override or {}
se.ai_recruit.max_experience = se.ai_recruit.max_experience or 9

local quality_cache = {}

local function any_faction()
	local cm, err = cm_()
	if not cm then return nil, err end
	local ok, f = pcall(function() return cm:query_model():world():faction_list():item_at(0) end)
	if not ok or is_null(f) then return nil, "no faction to reach the database through" end
	return f
end

-- se.query.unit_quality(unit_key) -> rows, best
--   rows = { { group, quality, quality_at_max_xp }, ... } from the game's live
--   cdir_military_generator_unit_qualities table (the campaign AI's own unit ranking; one row per
--   role group the unit belongs to). best = the highest quality over the groups, or the value in
--   se.ai_recruit.quality_override. Unknown unit: {}, 0. Cached per unit key.
function se.query.unit_quality(unit_key)
	unit_key = str(unit_key)
	local hit = quality_cache[unit_key]
	if not hit then
		local okn, err = need("se_unit_quality")
		if not okn then return nil, err end
		local f, e2 = any_faction()
		if not f then return nil, e2 end
		local s, msg = se_unit_quality(f, unit_key)
		if msg then return nil, msg end
		hit = {}
		for group, q, qmax in str(s):gmatch("([^=;]+)=([^,;]+),([^;]*);") do
			hit[#hit + 1] = { group = group, quality = tonumber(q) or 0, quality_at_max_xp = tonumber(qmax) or 0 }
		end
		quality_cache[unit_key] = hit
	end
	local override = se.ai_recruit.quality_override[unit_key]
	local best = 0
	for _, r in ipairs(hit) do if r.quality > best then best = r.quality end end
	if override then best = num(override) or best end
	return hit, best
end

-- quality of `candidate` in a role group it shares with `current` (nil when they share none)
local function shared_quality(current_rows, candidate_key)
	local rows = se.query.unit_quality(candidate_key)
	if not rows then return nil end
	local override = se.ai_recruit.quality_override[candidate_key]
	local best
	for _, c in ipairs(rows) do
		for _, r in ipairs(current_rows) do
			if r.group == c.group then
				local q = override and num(override) or c.quality
				if not best or q > best then best = q end
			end
		end
	end
	return best
end

-- se.ai_recruit.trace(on) -> was_on : let the DLL copy the AI's recruitment requests after every
--   planning pass (FUN_141cf8fe0). Off by default; switching it off drops what was collected.
function se.ai_recruit.trace(on)
	local okn, err = need("se_ai_recruit_trace")
	if not okn then return nil, err end
	return se_ai_recruit_trace(on and true or false)
end

-- se.query.ai_recruitment() -> passes (drains what was collected since the last call)
--   pass = { seq, faction_id, pending, requests = { { money, budget2, turn, target, rows = {
--   { id, cost, cost2, kind } } } } }. money / budget2 = what the planner set aside for that
--   request; rows = the purchases it priced; target = class of the request's target object (hex
--   rva of its vtable); faction_id = engine id of the planning faction (0 when not resolved).
function se.query.ai_recruitment()
	local okn, err = need("se_ai_recruit_passes")
	if not okn then return nil, err end
	local passes, pass, request = {}, nil, nil
	for rec in str(se_ai_recruit_passes()):gmatch("([^;]+);") do
		local f = {}
		for v in rec:gmatch("([^,]+)") do f[#f + 1] = v end
		if f[1] == "P" then
			pass = { seq = tonumber(f[2]), faction_id = tonumber(f[3]), pending = tonumber(f[4]), requests = {} }
			passes[#passes + 1] = pass
		elseif f[1] == "R" and pass then
			request = { money = tonumber(f[2]), budget2 = tonumber(f[3]), turn = tonumber(f[4]), target = f[5], rows = {} }
			pass.requests[#pass.requests + 1] = request
		elseif f[1] == "W" and request then
			request.rows[#request.rows + 1] = { id = tonumber(f[2]), cost = tonumber(f[3]), cost2 = tonumber(f[4]), kind = tonumber(f[5]) }
		end
	end
	return passes
end

-- se.ai_recruit.report(faction_key [, opts]) -> report
--   report = { faction, treasury, forces = { { cqi, characters = { { cqi, slots = { {
--     index, unit, experience, strength, recruiting, quality, effective,
--     best = { key, quality, cost, turns } | nil, gap, affordable } } } } } },
--     slots, empty, upgradable, affordable }
--   For every retinue slot of every army: the unit in it, its quality (effective = scaled towards
--   quality_at_max_xp by experience, which is how the AI's own table values a veteran) and the
--   best unit of the same role group the slot could recruit right now with no lock reason.
--   gap = best.quality / effective (> 1 means a better unit is available).
--   opts.min_gap (default 1.0) only fills `best` when the gap is at least that.
function se.ai_recruit.report(faction_key, opts)
	opts = opts or {}
	local cm, err = cm_()
	if not cm then return nil, err end
	local okf, faction = pcall(function() return cm:query_faction(str(faction_key)) end)
	if not okf or is_null(faction) then return nil, "unknown faction " .. str(faction_key) end
	local okt, treasury = pcall(function() return faction:treasury() end)
	local report = { faction = str(faction_key), treasury = okt and num(treasury) or 0, forces = {}, slots = 0, empty = 0, upgradable = 0, affordable = 0 }
	local okl, forces = pcall(function() return faction:military_force_list() end)
	if not okl or not forces then return report end
	for i = 0, forces:num_items() - 1 do
		local force = forces:item_at(i)
		local okc, chars = pcall(function() return force:character_list() end)
		local okq, fcqi = pcall(function() return force:command_queue_index() end)
		local frow = { cqi = okq and num(fcqi) or 0, characters = {} }
		if okc and chars then
			for j = 0, chars:num_items() - 1 do
				local okx, cqi = pcall(function() return chars:item_at(j):command_queue_index() end)
				local retinue = okx and se.query.retinue(cqi) or nil
				if retinue then
					local crow = { cqi = num(cqi), slots = {} }
					for _, slot in ipairs(retinue) do
						report.slots = report.slots + 1
						local srow = { index = slot.index, slot_cqi = slot.slot_cqi, unit = slot.unit_key, experience = slot.experience, strength = slot.strength, recruiting = slot.is_recruiting and slot.recruiting or nil }
						if not slot.unit_key or slot.unit_key == "" then
							report.empty = report.empty + 1
						elseif not slot.is_recruiting then
							local rows, best_q = se.query.unit_quality(slot.unit_key)
							srow.quality = best_q or 0
							local top = 0
							for _, r in ipairs(rows or {}) do if r.quality_at_max_xp > top then top = r.quality_at_max_xp end end
							local xp = math.min(num(slot.experience) or 0, se.ai_recruit.max_experience) / se.ai_recruit.max_experience
							srow.effective = srow.quality + math.max(0, top - srow.quality) * xp
							local options = se.query.recruitable(cqi, slot.index)
							local best
							for _, o in ipairs(options or {}) do
								if num(o.reasons) == 0 and o.key ~= slot.unit_key and o.key ~= "" then
									local q = shared_quality(rows or {}, o.key)
									if q and (not best or q > best.quality) then best = { key = o.key, quality = q, cost = num(o.cost) or 0, turns = num(o.turns) or 0 } end
								end
							end
							if best and srow.effective > 0 then
								srow.gap = best.quality / srow.effective
								if srow.gap >= (num(opts.min_gap) or 1.0) and srow.gap > 1.0 then
									srow.best = best
									report.upgradable = report.upgradable + 1
									srow.affordable = best.cost <= report.treasury
									if srow.affordable then report.affordable = report.affordable + 1 end
								end
							end
						end
						crow.slots[#crow.slots + 1] = srow
					end
					frow.characters[#frow.characters + 1] = crow
				end
			end
		end
		report.forces[#report.forces + 1] = frow
	end
	return report
end

----------------------------------------------------------------------------------------------
-- AI recruitment policy: better armies that fit their general (DLL 0.37+)
--
-- The engine only ever fills EMPTY retinue slots with the cheapest unit it can pay for. This
-- pass runs at the start of every AI faction's turn (inside the model callback) and
--   * fills empty slots with the best-scoring unit the slot can recruit, and
--   * replaces the unit of an occupied slot when another unit scores clearly higher,
-- at normal cost, within a budget. score = quality x element weight: a Wood general prefers
-- Wood units, then Metal, ... (se.ai_recruit.element_order). Everything is a plain table that a
-- script may edit before or after se.ai_recruit.enable().
----------------------------------------------------------------------------------------------

-- best to worst per general element. An entry is "element" or "element:class" with class =
-- "cavalry" | "infantry" (se.ai_recruit.class_of); a unit takes the best entry it matches, so
-- Earth and Fire generals rank Water CAVALRY (horse archers) above the infantry elements while
-- Water infantry stays where the plain "water" entry puts it.
se.ai_recruit.element_order = se.ai_recruit.element_order or {
	wood  = { "wood", "metal", "water", "earth", "fire" },
	metal = { "metal", "wood", "water", "earth", "fire" },
	water = { "water", "earth", "metal", "wood", "fire" },
	fire  = { "fire", "earth", "water:cavalry", "metal", "wood", "water" },
	earth = { "earth", "fire", "water:cavalry", "water", "metal", "wood" },
}
-- score multiplier by rank in that list (1 = the general's favourite). A list longer than this
-- table is spread evenly between the table's first and last value.
se.ai_recruit.element_weight = se.ai_recruit.element_weight or { 1.30, 1.15, 1.00, 0.90, 0.80 }
-- unit_class[unit_key] = "cavalry" | "infantry" for units the quality table does not know
se.ai_recruit.unit_class = se.ai_recruit.unit_class or {}
-- unit_element[unit_key] = "wood" ... for keys that do not carry their element
se.ai_recruit.unit_element = se.ai_recruit.unit_element or {}

se.ai_recruit.config = se.ai_recruit.config or {
	fill_empty = true,          -- choose the unit for empty slots (else the engine's cheapest pick stays)
	replace = true,             -- replace units in occupied slots
	min_gain = 1.5,             -- replace only when new score >= old score x this
	same_role_only = false,     -- true: a replacement must share a role group with the old unit
	min_strength = 50,          -- do not replace a unit below this strength % (let it replenish or die)
	max_per_character = 2,      -- orders per character per turn
	max_per_force = 3,          -- orders per army per turn
	max_per_faction = 8,        -- orders per faction per turn
	reserve = 1500,             -- treasury the pass never touches
	income_turns = 3,           -- and it spends at most projected income x this per turn ...
	max_spend = 4000,           -- ... and at most this much per turn
	min_income = 0,             -- a faction whose projected income is below this does nothing
	duplicate_penalty = 0.92,   -- score multiplier per copy of the same unit already in the retinue
	max_copies = 0,             -- hard limit of copies of one unit per retinue (0 = no limit)
	log_orders = true,
}

local ELEMENTS = { wood = true, metal = true, water = true, earth = true, fire = true }

-- se.ai_recruit.element_of(key) -> "wood" | "metal" | "water" | "earth" | "fire" | nil
--   From a unit key or a character subtype key ("3k_general_wood", "3k_main_unit_wood_ji_militia",
--   "rew_iro_regional_yang_wood_yang_guardians"): the first "_"-separated word that names an
--   element. se.ai_recruit.unit_element overrides it.
function se.ai_recruit.element_of(key)
	key = str(key)
	local forced = se.ai_recruit.unit_element[key]
	if forced then return forced end
	for word in key:gmatch("[^_]+") do
		if ELEMENTS[word] then return word end
	end
	return nil
end

-- se.ai_recruit.class_of(unit_key) -> "cavalry" | "infantry"
--   cavalry = any of the unit's role groups in the game's quality table names cavalry
--   (Default_land_cavalry_melee / _shock / _missile ...); everything else counts as infantry.
function se.ai_recruit.class_of(unit_key)
	unit_key = str(unit_key)
	local forced = se.ai_recruit.unit_class[unit_key]
	if forced then return forced end
	for _, r in ipairs(se.query.unit_quality(unit_key) or {}) do
		if str(r.group):lower():match("cavalry") then return "cavalry" end
	end
	return "infantry"
end

local function rank_weight(rank, count)
	local w = se.ai_recruit.element_weight
	if #w >= count then return num(w[rank]) or 1 end
	if #w == 0 then return 1 end
	if count <= 1 then return num(w[1]) or 1 end
	local first, last = num(w[1]) or 1, num(w[#w]) or 1
	return first + (last - first) * (rank - 1) / (count - 1)
end

-- se.ai_recruit.element_factor(general_element, unit_key) -> multiplier (1 when unknown)
--   The weight of the best entry of the general's list that the unit matches.
function se.ai_recruit.element_factor(general_element, unit_key)
	local order = general_element and se.ai_recruit.element_order[general_element]
	local element = se.ai_recruit.element_of(unit_key)
	if not order or not element then return 1 end
	local class
	for rank, entry in ipairs(order) do
		local e, c = str(entry):match("^([^:]+):?(.*)$")
		if e == element then
			if c == "" then return rank_weight(rank, #order) end
			class = class or se.ai_recruit.class_of(unit_key)
			if c == class then return rank_weight(rank, #order) end
		end
	end
	return 1
end

-- forces of a faction as plain data: { { cqi, characters = { { cqi, element } } } }
-- (kept separate so a test, or another script, can replace it)
function se.ai_recruit.forces_of(faction_key)
	local cm, err = cm_()
	if not cm then return nil, err end
	local okf, faction = pcall(function() return cm:query_faction(str(faction_key)) end)
	if not okf or is_null(faction) then return nil, "unknown faction " .. str(faction_key) end
	local out = {}
	local okl, forces = pcall(function() return faction:military_force_list() end)
	if not okl or not forces then return out end
	for i = 0, forces:num_items() - 1 do
		local force = forces:item_at(i)
		local okq, fcqi = pcall(function() return force:command_queue_index() end)
		local row = { cqi = okq and num(fcqi) or 0, characters = {} }
		local okc, chars = pcall(function() return force:character_list() end)
		if okc and chars then
			for j = 0, chars:num_items() - 1 do
				local c = chars:item_at(j)
				local okx, cqi = pcall(function() return c:command_queue_index() end)
				local oks, subtype = pcall(function() return c:character_subtype_key() end)
				if okx then row.characters[#row.characters + 1] = { cqi = num(cqi), element = oks and se.ai_recruit.element_of(subtype) or nil } end
			end
		end
		out[#out + 1] = row
	end
	return out
end

local function faction_money(faction_key)
	local cm = cm_()
	if not cm then return 0, 0 end
	local ok, f = pcall(function() return cm:query_faction(str(faction_key)) end)
	if not ok or is_null(f) then return 0, 0 end
	local okt, t = pcall(function() return f:treasury() end)
	local oki, inc = pcall(function() return f:projected_net_income() end)
	return okt and num(t) or 0, oki and num(inc) or 0
end

-- se.ai_recruit.plan(faction_key [, money]) -> orders, info        (changes nothing)
--   orders = { { op = "recruit" | "replace", force, character, general (element), slot, unit, cost, old, score,
--   old_score, gain } } sorted by gain, already cut to the budget and the per-turn caps.
--   money = { treasury, income } overrides the faction's own numbers (tests, what-if).
function se.ai_recruit.plan(faction_key, money)
	local cfg = se.ai_recruit.config
	local forces, err = se.ai_recruit.forces_of(faction_key)
	if not forces then return nil, err end
	local treasury, income
	if money then treasury, income = num(money.treasury) or 0, num(money.income) or 0 else treasury, income = faction_money(faction_key) end
	local info = { faction = str(faction_key), treasury = treasury, income = income, candidates = 0, budget = 0 }
	if income < cfg.min_income then info.skipped = "income below min_income" return {}, info end
	local budget = math.min(treasury - cfg.reserve, math.max(0, income) * cfg.income_turns, cfg.max_spend)
	info.budget = math.max(0, budget)
	if budget <= 0 then info.skipped = "no budget" return {}, info end

	local candidates = {}
	for _, force in ipairs(forces) do
		for _, ch in ipairs(force.characters) do
			local retinue = se.query.retinue(ch.cqi)
			if retinue then
				-- captains (and other leaders whose subtype key names no element) take the element
				-- of their own bodyguard unit in slot 0 ("..._wood_ji_infantry_captain"): in a
				-- 50-turn run it matched the subtype's element for 266 of 266 generals
				if not ch.element then
					for _, s in ipairs(retinue) do
						if s.index == 0 and s.unit_key and s.unit_key ~= "" then ch.element = se.ai_recruit.element_of(s.unit_key) end
					end
				end
				local copies = {}
				for _, s in ipairs(retinue) do
					local k = (s.is_recruiting and s.recruiting ~= "" and s.recruiting) or s.unit_key
					if k and k ~= "" then copies[k] = (copies[k] or 0) + 1 end
				end
				for _, s in ipairs(retinue) do
					local empty = not s.unit_key or s.unit_key == ""
					local wanted = (empty and cfg.fill_empty) or (not empty and cfg.replace and (num(s.strength) or 100) >= cfg.min_strength)
					if wanted and not s.is_recruiting and s.can_recruit ~= false then
						local old_rows, old_q, old_score = {}, 0, 0
						if not empty then
							old_rows, old_q = se.query.unit_quality(s.unit_key)
							old_rows, old_q = old_rows or {}, old_q or 0
							local top = 0
							for _, r in ipairs(old_rows) do if r.quality_at_max_xp > top then top = r.quality_at_max_xp end end
							local xp = math.min(num(s.experience) or 0, se.ai_recruit.max_experience) / se.ai_recruit.max_experience
							old_score = (old_q + math.max(0, top - old_q) * xp) * se.ai_recruit.element_factor(ch.element, s.unit_key)
						end
						-- an occupied slot whose unit has no known quality cannot be judged: leave it
						if empty or old_score > 0 then
							local best
							for _, o in ipairs(se.query.recruitable(ch.cqi, s.index) or {}) do
								local limit = num(cfg.max_copies) or 0
								if num(o.reasons) == 0 and o.key ~= "" and o.key ~= s.unit_key and (num(o.cost) or 0) <= budget
									and (limit <= 0 or (copies[o.key] or 0) < limit) then
									local q
									if not empty and cfg.same_role_only then q = shared_quality(old_rows, o.key) else local _, b = se.query.unit_quality(o.key) q = b end
									if q and q > 0 then
										local score = q * se.ai_recruit.element_factor(ch.element, o.key) * (cfg.duplicate_penalty ^ (copies[o.key] or 0))
										if not best or score > best.score or (score == best.score and o.key < best.unit) then
											best = { unit = o.key, cost = num(o.cost) or 0, score = score }
										end
									end
								end
							end
							if best and (empty or best.score >= old_score * cfg.min_gain) then
								candidates[#candidates + 1] = { op = empty and "recruit" or "replace", force = force.cqi, character = ch.cqi, slot = s.index, general = ch.element,
									unit = best.unit, cost = best.cost, copies = copies[best.unit] or 0, old = (not empty) and s.unit_key or nil, score = best.score, old_score = old_score,
									gain = empty and best.score or (best.score / old_score) }
							end
						end
					end
				end
			end
		end
	end
	info.candidates = #candidates
	-- replacements with the biggest gain first, then the empty slots by score; ties by ids so
	-- every machine of a multiplayer game takes the same orders
	table.sort(candidates, function(a, b)
		if a.op ~= b.op then return a.op == "replace" end
		if a.gain ~= b.gain then return a.gain > b.gain end
		if a.character ~= b.character then return a.character < b.character end
		return a.slot < b.slot
	end)
	local orders, per_char, per_force, picked = {}, {}, {}, {}
	for _, c in ipairs(candidates) do
		if #orders >= cfg.max_per_faction then break end
		if c.cost <= budget and (per_char[c.character] or 0) < cfg.max_per_character and (per_force[c.force] or 0) < cfg.max_per_force then
			-- the duplicate penalty was computed per slot: do not hand one character the same
			-- new unit more than once per turn beyond what the penalty allowed for
			local tag = c.character .. "|" .. c.unit
			picked[tag] = (picked[tag] or 0) + 1
			local limit = num(cfg.max_copies) or 0
			if picked[tag] <= 2 and (limit <= 0 or (c.copies or 0) + picked[tag] <= limit) then
				budget = budget - c.cost
				per_char[c.character] = (per_char[c.character] or 0) + 1
				per_force[c.force] = (per_force[c.force] or 0) + 1
				orders[#orders + 1] = c
			end
		end
	end
	info.spent = info.budget - math.max(0, budget)
	return orders, info
end

-- se.ai_recruit.execute(orders) -> done, failed : carries the orders out at normal cost
function se.ai_recruit.execute(orders)
	local done, failed = 0, 0
	for _, o in ipairs(orders or {}) do
		local ok, msg
		if o.op == "replace" then
			ok, msg = se.modify.replace(o.character, o.slot, o.unit, {})
		else
			ok, msg = se.modify.recruit(o.character, o.unit, { slot = o.slot })
		end
		if ok then done = done + 1 else failed = failed + 1 end
		if se.ai_recruit.config.log_orders then
			log(string.format("ai_recruit %s: char %s (%s general) slot %s %s%s (cost %s, score %.0f%s) -> %s %s", str(o.op), str(o.character), str(o.general or "?"), str(o.slot),
				o.old and (o.old .. " -> ") or "", str(o.unit), str(o.cost), o.score or 0, o.old and string.format(", was %.0f, x%.2f", o.old_score or 0, o.gain or 0) or "", str(ok), str(msg or "")))
		end
	end
	return done, failed
end

-- se.ai_recruit.set_policy(fn) : fn(faction_key) -> orders replaces se.ai_recruit.plan as the
--   decision maker of the turn-start pass (it may call plan() and edit the result). nil = default.
function se.ai_recruit.set_policy(fn)
	se.ai_recruit.policy = fn
	return true
end

-- se.ai_recruit.enable() -> ok, message : install the turn-start pass for AI factions (once per
--   Lua state; call it again after a load). Runs inside the model callback, the same on every
--   machine of a multiplayer game: no local-player branching, no randomness.
-- se.ai_recruit.disable() : the listener stays registered but does nothing.
function se.ai_recruit.enable()
	local core = se.core or G("core")
	if type(core) ~= "table" then return false, "core (event manager) is not available: set se.core = core first" end
	se.ai_recruit.enabled = true
	if se.ai_recruit.listening then return true, "already listening; enabled" end
	se.ai_recruit.listening = true
	core:add_listener("se_ai_recruit_turn_start", "FactionTurnStart", function(context)
		if not se.ai_recruit.enabled then return false end
		local ok, r = pcall(function() local f = context:faction() return not f:is_human() and not f:is_dead() end)
		return ok and r
	end, function(context)
		local key = context:faction():name()
		local okp, orders, info = pcall(se.ai_recruit.policy or se.ai_recruit.plan, key)
		if not okp then log("ai_recruit " .. key .. ": policy error: " .. str(orders)) return end
		if orders and #orders > 0 then
			local done, failed = se.ai_recruit.execute(orders)
			log(string.format("ai_recruit %s: %d orders done, %d failed (budget %s, spent %s, %s candidates)", key, done, failed,
				str(info and info.budget), str(info and info.spent), str(info and info.candidates)))
		end
	end, true)
	return true, "listening"
end

function se.ai_recruit.disable()
	se.ai_recruit.enabled = false
	return true
end

----------------------------------------------------------------------------------------------
-- AI skill allocation (DLL 0.48 feat/ai-campaign): spend AI characters' skill points by role
----------------------------------------------------------------------------------------------
-- The vanilla CAI weighs skills with 4 utilization hints for ~800 skills and all policy weights at
-- 1.0 (notes/AI_CAMPAIGN.md), i.e. close to uniform. CA (and 190E) tag most skills with dummy
-- effects 3k_main_effect_ai_hint_general / _governor / _minister; this policy reads them live
-- from the node's next level (se.query.skill_tree next1..3) and spends each unspent point on the
-- best node the engine allows (node.can_add: links, level caps and unlock requirements are the
-- engine's own check, so 190E culture trees need no special case).
--   score = role factor (tag for the character's role: role_match; only other roles: role_other;
--           untagged: untagged) x element_match when the skill key carries the character's element
--           x continue_bonus when the node already has points x skill_weight[skill or node key]
-- Deterministic: ties break on (row, col); no clocks, addresses or random numbers. AI factions only.
se.ai_skills = se.ai_skills or {}
se.ai_skills.config = se.ai_skills.config or {
	role_match = 3.0, role_other = 0.4, untagged = 1.0,
	element_match = 1.25, continue_bonus = 1.2,
	idle_role = "general",           -- characters without a force or a post are scored as this role
	max_per_character = 3,           -- points spent per character per trigger
	max_per_faction = 40,            -- per FactionTurnStart sweep
	on_point_available = true,       -- also spend at CharacterSkillPointAvailable (AI characters)
	exclude = { "personal_goal", "vow", "deed", "mission" }, -- node/skill keys containing these are never bought
	log_orders = true,
}
se.ai_skills.skill_weight = se.ai_skills.skill_weight or {}
local ELEMENT_WORDS = { wood = true, fire = true, earth = true, metal = true, water = true }

local function key_element(k)
	for w in str(k):gmatch("[^_]+") do if ELEMENT_WORDS[w] then return w end end
	return nil
end

local function key_has(k, sub)
	-- substring test for keys made of [%w_] only (no magic characters), via match: CA's
	-- string.find takes only 2 args here and string.sub has a sticky error flag (avoid both).
	return str(k):match(sub) ~= nil
end

-- role of a QUERY_CHARACTER: "general" | "governor" | "minister" | config.idle_role
function se.ai_skills.role_of(ch)
	local okf, f = pcall(function() return ch:has_military_force() end)
	if okf and f then return "general" end
	local okp, post = pcall(function() return ch:character_post() end)
	local okn, null = pcall(function() return post == nil or post:is_null_interface() end)
	if okp and okn and not null then
		local okk, k = pcall(function() return post:ministerial_position_record_key() end)
		if okk and key_has(str(k), "governor") then return "governor" end
		return "minister"
	end
	return se.ai_skills.config.idle_role or "general"
end

local function node_tags(n)
	local tags = {}
	for _, f in ipairs({ n.next1, n.next2, n.next3 }) do
		for k in str(f or ""):gmatch("[^,]+") do
			for _, r in ipairs({ "general", "governor", "minister" }) do
				if key_has(k, "ai_hint_" .. r) then tags[r] = true end
			end
		end
	end
	return tags
end

function se.ai_skills.score(n, role, element)
	local c = se.ai_skills.config
	local key = (n.skill ~= nil and n.skill ~= "") and n.skill or str(n.node)
	for _, x in ipairs(c.exclude or {}) do
		if key_has(key, x) or key_has(str(n.node), x) then return 0 end
	end
	-- the actual bonuses first: effects of the level this point buys (generated data, else the live
	-- next-level keys), weighted for the role; the ai_hint tags only when no effect is known
	local effects = nil
	local sk = se.ai_effects and se.ai_effects.skill and se.ai_effects.skill[key]
	if type(sk) == "table" then effects = sk[(n.level or 0) + 1] end
	if not effects then
		effects = {}
		for _, f in ipairs({ n.next1, n.next2, n.next3 }) do
			for k in str(f or ""):gmatch("[^,]+") do effects[#effects + 1] = k end
		end
	end
	local fit = se.ai_effects_fit and se.ai_effects_fit(effects, role)
	local s
	if fit ~= nil then
		s = c.role_other + (c.role_match - c.role_other) * fit
	else
		local tags = node_tags(n)
		if tags[role] then s = c.role_match elseif tags.general or tags.governor or tags.minister then s = c.role_other else s = c.untagged end
	end
	if element and key_element(key) == element then s = s * c.element_match end
	if (n.level or 0) > 0 then s = s * c.continue_bonus end
	s = s * (se.ai_skills.skill_weight[key] or se.ai_skills.skill_weight[str(n.node)] or 1)
	return s, key
end

-- se.ai_skills.best(cqi, ch) -> best node, score, role | nil: pure, reads the tree once
function se.ai_skills.best(cqi, ch)
	local t = se.query.skill_tree(cqi)
	if type(t) ~= "table" or (t.points or 0) <= 0 then return nil end
	local role = se.ai_skills.role_of(ch)
	local okk, sub = pcall(function() return ch:character_subtype_key() end)
	local element = okk and key_element(sub) or nil
	local best, bs, bkey = nil, 0, nil
	for _, n in ipairs(t.nodes or {}) do
		if n.can_add then
			local s, key = se.ai_skills.score(n, role, element)
			if s > bs or (s == bs and best and s > 0 and (n.row < best.row or (n.row == best.row and n.col < best.col))) then
				best, bs, bkey = n, s, key
			end
		end
	end
	if not best then return nil end
	return best, bs, role, bkey, t.points
end

-- se.ai_skills.allocate(cqi) -> points spent: up to max_per_character points, one at a time
-- (the tree is re-read after each point, so children unlocked by a purchase are considered).
function se.ai_skills.allocate(cqi)
	local ch = se.character(cqi)
	if not ch then return 0 end
	local spent = 0
	for _ = 1, se.ai_skills.config.max_per_character or 3 do
		local n, s, role, key = se.ai_skills.best(cqi, ch)
		if not n then break end
		local ok, msg = se.modify.skill_add(cqi, n.row, n.col)
		if se.ai_skills.config.log_orders then
			log(string.format("ai_skills: char %s (%s) %s (%d,%d) score %.2f -> %s %s", str(cqi), str(role), str(key), n.row, n.col, s, str(ok), str(msg or "")))
		end
		if not ok then break end
		spent = spent + 1
	end
	return spent
end

local function ai_faction(f)
	local ok, r = pcall(function() return not f:is_human() and not f:is_dead() end)
	return ok and r
end

-- se.ai_skills.enable() / disable(): install the triggers (once per Lua state; call again after a load)
function se.ai_skills.enable()
	local core = se.core or G("core")
	if type(core) ~= "table" then return false, "core (event manager) is not available: set se.core = core first" end
	local okn, err = need("se_skill_nodes", "se_skill_add")
	if not okn then return false, err end
	se.ai_skills.enabled = true
	if se.ai_skills.listening then return true, "already listening; enabled" end
	se.ai_skills.listening = true
	core:add_listener("se_ai_skills_turn_start", "FactionTurnStart", function(context)
		return se.ai_skills.enabled and ai_faction(context:faction())
	end, function(context)
		local f = context:faction()
		local okl, list = pcall(function() return f:character_list() end)
		if not okl then return end
		local total, cap = 0, se.ai_skills.config.max_per_faction or 40
		for i = 0, list:num_items() - 1 do
			if total >= cap then break end
			local okc, cqi = pcall(function() return list:item_at(i):cqi() end)
			if okc then
				local okp, n = pcall(se.ai_skills.allocate, cqi)
				if okp then total = total + n else log("ai_skills: error " .. str(n)) end
			end
		end
		if total > 0 then log(string.format("ai_skills %s: %d points spent", str(f:name()), total)) end
	end, true)
	core:add_listener("se_ai_skills_point", "CharacterSkillPointAvailable", function(context)
		if not (se.ai_skills.enabled and se.ai_skills.config.on_point_available) then return false end
		local ok, r = pcall(function() return ai_faction(context:query_character():faction()) end)
		return ok and r
	end, function(context)
		local okc, cqi = pcall(function() return context:query_character():cqi() end)
		if okc then
			local okp, e = pcall(se.ai_skills.allocate, cqi)
			if not okp then log("ai_skills: error " .. str(e)) end
		end
	end, true)
	return true, "listening"
end

function se.ai_skills.disable()
	se.ai_skills.enabled = false
	return true
end

----------------------------------------------------------------------------------------------
-- AI equipment (DLL 0.48 feat/ai-campaign): put AI factions' unused items on their characters
----------------------------------------------------------------------------------------------
-- Stock script interfaces only (QUERY/MODIFY_CHARACTER_CEO_MANAGEMENT equip_ceo_in_slot). Per AI
-- FactionTurnStart: characters in priority order (faction leader, generals by rank, then the rest;
-- ties by cqi), each slot of an allowed category: an empty slot takes the best equippable item that
-- is not equipped anywhere; an occupied slot is upgraded only when an unequipped item is worth at
-- least min_gain more and the engine allows unequipping. Small batches (max_per_faction): mass CEO
-- work in AI turns once broke every script (31 Oct). Never touches categories outside `categories`
-- or items whose key matches `exclude` (personal goals, vows, deeds).
--   value(item) = ceo_value[key] or rank of rarity[key] or of the key's suffix
--                 (_common 1, _refined 2, _exceptional 3, _unique 4), else unknown_value.
se.ai_ceos = se.ai_ceos or {}
se.ai_ceos.config = se.ai_ceos.config or {
	categories = { "weapon", "armour", "mount", "accessory", "follower" }, -- category key must contain one
	exclude = { "personal_goal", "vow", "deed", "mission", "trait", "title" },
	min_gain = 1,            -- upgrade an equipped item only for at least this much more value
	min_fit = 0.5,           -- with effect data: skip items whose effects suit the character's role less than this
	unknown_value = 1.5,     -- items with no value / rarity
	max_per_faction = 6,     -- equips per faction turn
	log_orders = true,
}
se.ai_ceos.ceo_value = se.ai_ceos.ceo_value or {}  -- [ceo key] = number (mods)
se.ai_ceos.rarity = se.ai_ceos.rarity or {}        -- [ceo key] = "common" | "refined" | "exceptional" | "unique"
local RARITY_RANK = { common = 1, refined = 2, exceptional = 3, unique = 4 }

function se.ai_ceos.value(key)
	key = str(key)
	local v = se.ai_ceos.ceo_value[key]
	if type(v) == "number" then return v end
	local r = RARITY_RANK[se.ai_ceos.rarity[key] or ""]
	if r then return r end
	for _, name in ipairs({ "unique", "exceptional", "refined", "common" }) do -- fixed order (lockstep)
		if key:match("_" .. name .. "$") or key:match("_" .. name .. "_") then return RARITY_RANK[name] end
	end
	return se.ai_ceos.config.unknown_value
end

-- Effect data (generated from the mod's DB by tools\aicamp\gen_ai_effects.py and loaded by the mod):
--   se.ai_effects.effect_role[effect_key] = { general = w, governor = w, minister = w }   (0..1 each)
--   se.ai_effects.ceo[ceo_key]            = { effect_key, ... }
--   se.ai_effects.skill[skill_key]        = { [level] = { effect_key, ... } }
se.ai_effects = se.ai_effects or { effect_role = {}, ceo = {}, skill = {} }

-- fit of a list of effect keys for a role: share of the summed role weights, nil when nothing is known
function se.ai_effects_fit(effects, role)
	local er = se.ai_effects.effect_role or {}
	local sum, best = 0, 0
	for _, e in ipairs(effects or {}) do
		local w = er[e]
		if w then
			sum = sum + (w[role] or 0)
			best = best + math.max(w.general or 0, w.governor or 0, w.minister or 0)
		end
	end
	if best <= 0 then return nil end
	return sum / best
end

-- se.ai_ceos.score(ceo_key, role) = value(key) x (0.5 + fit) when effect data knows the item, else value(key);
-- 0 (not for this character) when fit < config.min_fit
function se.ai_ceos.score(key, role)
	local v = se.ai_ceos.value(key)
	local fit = se.ai_effects_fit(se.ai_effects.ceo and se.ai_effects.ceo[str(key)], role or "general")
	if fit == nil then return v end
	if fit < (se.ai_ceos.config.min_fit or 0) then return 0 end
	return v * (0.5 + fit)
end

local function category_allowed(cat)
	cat = str(cat)
	local c = se.ai_ceos.config
	for _, x in ipairs(c.exclude or {}) do if key_has(cat, x) then return false end end
	for _, x in ipairs(c.categories or {}) do if key_has(cat, x) then return true end end
	return false
end

local function item_allowed(key)
	for _, x in ipairs(se.ai_ceos.config.exclude or {}) do if key_has(str(key), x) then return false end end
	return true
end

-- Court posts of "prime-minister tier" (ministerial_positions.power_rating >= 10 in vanilla data):
-- they and the faction leader get first pick of minister-type items. Mods add keys here.
se.ai_ceos.prime_tier_posts = se.ai_ceos.prime_tier_posts or {
	["3k_main_court_offices_prime_minister"] = true, ["3k_dlc04_court_offices_prime_minister"] = true,
	["ep_court_offices_prime_minister"] = true, ["faction_heir"] = true,
	["3k_dlc04_court_offices_grand_tutor"] = true, ["3k_dlc04_court_offices_general_in_chief"] = true,
	["3k_dlc04_court_offices_grand_commandant"] = true, ["3k_dlc04_court_offices_grand_excellency_of_works"] = true,
	["3k_dlc04_court_offices_grand_excellency_over_the_masses"] = true,
}
-- Which character class gets first pick per slot category ("battle" = generals by rank; "court" = faction
-- leader + prime-tier posts, then other post holders). Categories not listed use "battle".
se.ai_ceos.category_class = se.ai_ceos.category_class or { weapon = "battle", armour = "battle", mount = "battle", accessory = "court", follower = "court" }

local function post_key(ch)
	local k = nil
	pcall(function()
		local p = ch:character_post()
		if p and not p:is_null_interface() then k = str(p:ministerial_position_record_key()) end
	end)
	return k
end

-- characters of a faction with their class tiers. battle_tier: 0 general (also a leader commanding
-- a force), 1 others. court_tier: 0 faction leader / prime-tier post, 1 other post holders, 2 others.
function se.ai_ceos.characters_of(f)
	local out = {}
	local okl, list = pcall(function() return f:character_list() end)
	if not okl then return out end
	local leader = -1
	pcall(function() leader = f:faction_leader():cqi() end)
	for i = 0, list:num_items() - 1 do
		local ch = list:item_at(i)
		local ok, row = pcall(function()
			local cqi = ch:cqi()
			local pk = post_key(ch)
			local court = (cqi == leader or (pk and se.ai_ceos.prime_tier_posts[pk])) and 0 or (pk and 1 or 2)
			return { cqi = cqi, ch = ch, battle_tier = ch:has_military_force() and 0 or 1, court_tier = court, rank = ch:rank(), post = pk }
		end)
		if ok then out[#out + 1] = row end
	end
	return out
end

local function class_of_category(cat)
	local words = {}
	for word in pairs(se.ai_ceos.category_class) do words[#words + 1] = word end
	table.sort(words) -- fixed order (lockstep)
	for _, word in ipairs(words) do
		if key_has(str(cat), word) then return se.ai_ceos.category_class[word] end
	end
	return "battle"
end

-- characters ordered for one category class; ties by rank (desc) then cqi (deterministic)
local function ordered(chars, class)
	local t = {}
	for i, c in ipairs(chars) do t[i] = c end
	local tier = class == "court" and "court_tier" or "battle_tier"
	table.sort(t, function(a, b)
		if a[tier] ~= b[tier] then return a[tier] < b[tier] end
		if a.rank ~= b.rank then return a.rank > b.rank end
		return a.cqi < b.cqi
	end)
	return t
end

-- se.ai_ceos.plan(faction_key) -> orders { {cqi, slot (index), category, ceo, old, value, old_value} }
-- Pure: reads the engine, changes nothing. Items are matched by ceo_data_key + command_queue_index.
function se.ai_ceos.plan(faction_key)
	local cm = cm_()
	if not cm then return {} end
	local f = cm:query_faction(faction_key)
	local orders, used = {}, {}
	local cap = se.ai_ceos.config.max_per_faction or 6
	local chars = se.ai_ceos.characters_of(f)
	-- one pass per class: battle gear to generals by rank first, court items to the leader and
	-- prime-tier posts first. Only items equipped NOWHERE are handed out: nothing is ever taken off
	-- another character.
	for _, class in ipairs({ "battle", "court" }) do
		for _, c in ipairs(ordered(chars, class)) do
			if #orders >= cap then break end
			-- role for scoring: battle gear always as a general; court items by the post
			-- (governor post -> governor, leader / other posts -> minister, no post -> general)
			local role = "general"
			if class == "court" and c.court_tier <= 1 then
				role = (c.post and key_has(c.post, "governor")) and "governor" or "minister"
			end
			local okm, slots = pcall(function() return c.ch:ceo_management():all_ceo_equipment_slots() end)
			if okm and slots then
				for s = 0, slots:num_items() - 1 do
					if #orders >= cap then break end
					local sl = slots:item_at(s)
					local okc, cat = pcall(function() return sl:category_key() end)
					if okc and category_allowed(cat) and class_of_category(cat) == class then
						local cur = nil
						pcall(function() local e = sl:equipped_ceo() if e and not e:is_null_interface() then cur = e end end)
						local cur_key = cur and str(cur:ceo_data_key()) or nil
						local cur_v = cur and se.ai_ceos.score(cur_key, role) or 0
						local can_swap = cur == nil or (cur_key and item_allowed(cur_key) and select(2, pcall(function() return sl:can_unequip_ceo() end)) == true)
						if can_swap then
							local best, bv, bid = nil, 0, nil
							local oko, opts = pcall(function() return sl:all_equippable_ceos() end)
							for k = 0, (oko and opts and opts:num_items() or 0) - 1 do
								local ceo = opts:item_at(k)
								local okk, key, eq, id = pcall(function() return str(ceo:ceo_data_key()), ceo:is_equipped_in_slot(), ceo:command_queue_index() end)
								if okk and not eq and not used[id] and item_allowed(key) then
									local v = se.ai_ceos.score(key, role)
									if v > bv or (v == bv and best and key < best) then best, bv, bid = key, v, id end
								end
							end
							if best and (cur == nil or bv >= cur_v + (se.ai_ceos.config.min_gain or 1)) then
								used[bid] = true
								orders[#orders + 1] = { cqi = c.cqi, slot = s, category = str(cat), ceo = best, ceo_id = bid, old = cur_key, value = bv, old_value = cur_v, role = role }
							end
						end
					end
				end
			end
		end
	end
	return orders
end

-- se.ai_ceos.execute(orders) -> done, failed. Re-finds slot and item by index / command_queue_index.
function se.ai_ceos.execute(orders)
	local done, failed = 0, 0
	local cm = cm_()
	for _, o in ipairs(orders or {}) do
		local ok, msg = pcall(function()
			local q = cm:query_character(o.cqi)
			local sl = q:ceo_management():all_ceo_equipment_slots():item_at(o.slot)
			local opts = sl:all_equippable_ceos()
			local item = nil
			for k = 0, opts:num_items() - 1 do
				local c = opts:item_at(k)
				if c:command_queue_index() == o.ceo_id then item = c break end
			end
			if not item then return "item gone" end
			cm:modify_character(o.cqi):ceo_management():equip_ceo_in_slot(sl, item)
			return "equipped"
		end)
		local good = ok and msg == "equipped"
		if good then done = done + 1 else failed = failed + 1 end
		if se.ai_ceos.config.log_orders then
			log(string.format("ai_ceos: char %s %s %s%s (%.1f%s) -> %s", str(o.cqi), o.category, o.old and (o.old .. " -> ") or "", o.ceo, o.value,
				o.old and string.format(", was %.1f", o.old_value) or "", str(ok and msg or ("error " .. str(msg)))))
		end
	end
	return done, failed
end

function se.ai_ceos.enable()
	local core = se.core or G("core")
	if type(core) ~= "table" then return false, "core (event manager) is not available: set se.core = core first" end
	se.ai_ceos.enabled = true
	if se.ai_ceos.listening then return true, "already listening; enabled" end
	se.ai_ceos.listening = true
	core:add_listener("se_ai_ceos_turn_start", "FactionTurnStart", function(context)
		return se.ai_ceos.enabled and ai_faction(context:faction())
	end, function(context)
		local key = context:faction():name()
		local okp, orders = pcall(se.ai_ceos.plan, key)
		if not okp then log("ai_ceos " .. str(key) .. ": plan error " .. str(orders)) return end
		if #orders > 0 then
			local d, fl = se.ai_ceos.execute(orders)
			log(string.format("ai_ceos %s: %d equipped, %d failed", str(key), d, fl))
		end
	end, true)
	return true, "listening"
end

function se.ai_ceos.disable()
	se.ai_ceos.enabled = false
	return true
end

----------------------------------------------------------------------------------------------
-- AI court appointments (DLL 0.48 feat/ai-campaign): fill AI court posts by the faction's needs
----------------------------------------------------------------------------------------------
-- Owner's design (8 Nov): the faction's CAI personality decides what it looks for (income-based ->
-- characters whose minister effects boost income, military-based -> military bonuses); for income it
-- checks which income type it has most of (peasantry / industry / commerce, from its buildings' GDP)
-- and prefers ministers whose effects boost that type. Needs se.ai_effects (generated data:
-- personality leans, building GDP sectors, CEO / skill effects with sector tags).
--   candidate value = sum over his minister-scope effects (traits/items via ceo_management():all_ceos(),
--     skills taken) of minister weight x factor; factor: the faction's main income type 2, "all" 1.5,
--     other income types 1, each economic factor x (1.5 - lean); military x (0.5 + lean); untyped 1.
-- Defaults: only EMPTY posts are filled (replace = false), governor posts and faction leader / heir
-- are left to the engine, generals commanding a force are not moved, at most max_changes per faction
-- per cycle, every `every_turns` turns (staggered by faction key). Deterministic.
se.ai_court = se.ai_court or {}
se.ai_court.config = se.ai_court.config or {
	every_turns = 5, max_changes = 2, replace = false, replace_gain = 1.5,
	skip_generals = true, skip_posts = { "governor", "faction_leader", "faction_heir" },
	log_orders = true,
}

local function key_hash(s)
	local h = 0
	for i = 1, #s do h = (h * 31 + s:byte(i)) % 1000003 end
	return h
end

-- se.ai_court.focus(f) -> { lean (0 income .. 1 military), sector = main income type | nil, gdp = {peasantry,industry,commerce} }
function se.ai_court.focus(f)
	local fkey = str(f:name())
	local lean = 0.5
	local p = se.query.cai_personality and select(1, se.query.cai_personality(fkey))
	if type(p) == "table" and se.ai_effects.personality then
		lean = se.ai_effects.personality[p.record_key] or se.ai_effects.personality[p.key] or lean
	end
	local gdp = { peasantry = 0, industry = 0, commerce = 0 }
	local B = se.ai_effects.building or {}
	pcall(function()
		local regions = f:region_list()
		for i = 0, regions:num_items() - 1 do
			local slots = regions:item_at(i):slot_list()
			for s = 0, slots:num_items() - 1 do
				local sl = slots:item_at(s)
				if sl:has_building() then
					local b = B[str(sl:building():name())]
					if b then for k, v in pairs(b) do if gdp[k] then gdp[k] = gdp[k] + v end end end
				end
			end
		end
	end)
	local sector, best = nil, 0
	for _, k in ipairs({ "peasantry", "industry", "commerce" }) do -- fixed order (lockstep)
		if gdp[k] > best then sector, best = k, gdp[k] end
	end
	return { lean = lean, sector = sector, gdp = gdp, personality = type(p) == "table" and p.record_key or nil }
end

-- effect keys a character brings to a post: his CEOs (traits, items, ...) and the skill levels he has
function se.ai_court.effects_of(ch)
	local out = {}
	local C, S = se.ai_effects.ceo or {}, se.ai_effects.skill or {}
	pcall(function()
		local all = ch:ceo_management():all_ceos()
		for i = 0, all:num_items() - 1 do
			for _, e in ipairs(C[str(all:item_at(i):ceo_data_key())] or {}) do out[#out + 1] = e end
		end
	end)
	local okc, cqi = pcall(function() return ch:cqi() end)
	local t = okc and se.query.skill_tree and se.query.skill_tree(cqi)
	if type(t) == "table" then
		for _, n in ipairs(t.nodes or {}) do
			local sk = S[str(n.skill)]
			if sk and (n.level or 0) > 0 then
				for l = 1, n.level do for _, e in ipairs(sk[l] or {}) do out[#out + 1] = e end end
			end
		end
	end
	return out
end

function se.ai_court.value(ch, focus)
	local R = se.ai_effects.effect_role or {}
	local lean = focus.lean or 0.5
	local v = 0
	for _, e in ipairs(se.ai_court.effects_of(ch)) do
		local w = R[e]
		if w and (w.minister or 0) > 0 then
			local f = 1
			if w.sector == "military" then f = 0.5 + lean
			elseif w.sector then
				f = (w.sector == focus.sector) and 2 or (w.sector == "all" and 1.5 or 1)
				f = f * (1.5 - lean)
			end
			v = v + w.minister * f
		end
	end
	return v
end

local function skip_post(k)
	for _, x in ipairs(se.ai_court.config.skip_posts or {}) do if key_has(str(k), x) then return true end end
	return false
end

-- se.ai_court.plan(faction_key) -> orders { {cqi, post, value, old_cqi, old_value} }, focus. Pure.
function se.ai_court.plan(faction_key)
	local cm = cm_()
	if not cm then return {} end
	local f = cm:query_faction(faction_key)
	local c = se.ai_court.config
	local focus = se.ai_court.focus(f)
	-- free candidates: no post, not the leader, (optionally) not commanding a force
	local leader = -1
	pcall(function() leader = f:faction_leader():cqi() end)
	local cands = {}
	pcall(function()
		local list = f:character_list()
		for i = 0, list:num_items() - 1 do
			local ch = list:item_at(i)
			local ok, row = pcall(function()
				local cqi = ch:cqi()
				if cqi == leader then return nil end
				if c.skip_generals and ch:has_military_force() then return nil end
				local p = ch:character_post()
				if p and not p:is_null_interface() then return nil end
				return { cqi = cqi, ch = ch, value = se.ai_court.value(ch, focus) }
			end)
			if ok and row and row.value > 0 then cands[#cands + 1] = row end
		end
	end)
	table.sort(cands, function(a, b) if a.value ~= b.value then return a.value > b.value end return a.cqi < b.cqi end)
	-- posts: prime tier first, then by key
	local posts = {}
	pcall(function()
		local pl = f:character_posts()
		for i = 0, pl:num_items() - 1 do
			local p = pl:item_at(i)
			local k = str(p:ministerial_position_record_key())
			if not skip_post(k) then
				posts[#posts + 1] = { key = k, p = p, prime = se.ai_ceos.prime_tier_posts[k] and 0 or 1 }
			end
		end
	end)
	table.sort(posts, function(a, b) if a.prime ~= b.prime then return a.prime < b.prime end return a.key < b.key end)
	local orders, ci = {}, 1
	for _, post in ipairs(posts) do
		if #orders >= (c.max_changes or 2) or ci > #cands then break end
		local okh, cur, max = pcall(function() return post.p:current_post_holders(), post.p:maximum_post_holders() end)
		if okh and cur < max then
			local best = cands[ci]
			ci = ci + 1
			orders[#orders + 1] = { cqi = best.cqi, post = post.key, value = best.value }
		elseif okh and c.replace then
			local okp, holders = pcall(function() return post.p:post_holders() end)
			if okp and holders and holders:num_items() > 0 then
				local h = holders:item_at(0)
				local hv = se.ai_court.value(h, focus)
				local best = cands[ci]
				if best.value >= hv * (c.replace_gain or 1.5) and best.value > hv then
					ci = ci + 1
					orders[#orders + 1] = { cqi = best.cqi, post = post.key, value = best.value, old_cqi = h:cqi(), old_value = hv }
				end
			end
		end
	end
	return orders, focus
end

function se.ai_court.execute(orders, focus)
	local cm = cm_()
	local done, failed = 0, 0
	for _, o in ipairs(orders or {}) do
		local ok, err = pcall(function() cm:modify_character(o.cqi):assign_to_post(o.post) end)
		if ok then done = done + 1 else failed = failed + 1 end
		if se.ai_court.config.log_orders then
			log(string.format("ai_court: char %s -> %s (value %.1f%s; lean %.2f, main income %s) -> %s", str(o.cqi), o.post, o.value,
				o.old_cqi and string.format(", replaces %s %.1f", str(o.old_cqi), o.old_value) or "", focus and focus.lean or -1,
				str(focus and focus.sector), ok and "ok" or ("error " .. str(err))))
		end
	end
	return done, failed
end

function se.ai_court.enable()
	local core = se.core or G("core")
	if type(core) ~= "table" then return false, "core (event manager) is not available: set se.core = core first" end
	se.ai_court.enabled = true
	if se.ai_court.listening then return true, "already listening; enabled" end
	se.ai_court.listening = true
	core:add_listener("se_ai_court_turn_start", "FactionTurnStart", function(context)
		if not (se.ai_court.enabled and ai_faction(context:faction())) then return false end
		local okt, turn = pcall(function() return cm_():query_model():turn_number() end)
		local every = se.ai_court.config.every_turns or 5
		return okt and (turn + key_hash(str(context:faction():name()))) % every == 0
	end, function(context)
		local key = context:faction():name()
		local okp, orders, focus = pcall(se.ai_court.plan, key)
		if not okp then log("ai_court " .. str(key) .. ": plan error " .. str(orders)) return end
		if #orders > 0 then se.ai_court.execute(orders, focus) end
	end, true)
	return true, "listening"
end

function se.ai_court.disable()
	se.ai_court.enabled = false
	return true
end

-- se.ai_skills.trace(on): read-only log of every skill point an AI character spends (vanilla or
-- ours): "[se] ai_skill_pick|faction|cqi|subtype|role|skill_key". Summarise with
-- tools\aicamp\ai_campaign_summary.py (role fit by ai_hint tags).
function se.ai_skills.trace(on)
	se.ai_skills.tracing = on and true or false
	if se.ai_skills.trace_listening or not on then return true end
	local core = se.core or G("core")
	if type(core) ~= "table" then return false, "core (event manager) is not available" end
	se.ai_skills.trace_listening = true
	core:add_listener("se_ai_skills_trace", "CharacterSkillPointAllocated", function(context)
		if not se.ai_skills.tracing then return false end
		local ok, r = pcall(function() return ai_faction(context:query_character():faction()) end)
		return ok and r
	end, function(context)
		pcall(function()
			local ch = context:query_character()
			log(string.format("ai_skill_pick|%s|%s|%s|%s|%s", str(ch:faction():name()), str(ch:cqi()), str(ch:character_subtype_key()),
				str(se.ai_skills.role_of(ch)), str(context:skill_key())))
		end)
	end, true)
	return true
end

----------------------------------------------------------------------------------------------
-- save chunking (DLL 0.42+): saved strings above the engine's 64 KiB cap
----------------------------------------------------------------------------------------------
-- context:save_string() keeps at most ~65,536 bytes. cm:saving_game writes the whole
-- cm.saved_values store as ONE string, so once it grows past the cap the save holds a cut string,
-- loadstring fails on the next load and load_values_from_string throws every mod's saved values
-- away. Here campaign_manager:save_named_value / load_named_value are wrapped: a string or table
-- that serialises above THRESHOLD bytes is written as numbered slots "<name>__se_<i>" plus a marker
-- "--se_chunks:<n>:<len>" in its own slot, and joined again on load. Old saves carry no marker and
-- load as before. Without the DLL a chunked value loads as empty (the same wipe as before).
-- Installed automatically whenever the campaign manager class appears in this state;
-- script_extender.cfg save_chunking=0 turns it off.

se.saves = se.saves or {}
local SV = se._saves or { installed = false, path = "not installed", chunked_saves = 0, chunked_loads = 0, last_name = "", last_len = 0, logged = {} }
se._saves = SV
local CHUNK = 60000 -- size of one numbered slot
-- values up to this size are written in one slot, as without chunking (the cap: 65,476 bytes
-- survived, 65,796 did not)
local THRESHOLD = 64000
local MARK = "--se_chunks:"

local function chunking_on()
	local f = G("se_save_chunking")
	if type(f) ~= "function" then return true end
	local ok, on = pcall(f)
	return not ok or on ~= false
end

local function wrap_save(orig)
	return function(self, name, value, ...)
		if type(name) == "string" and (type(value) == "string" or type(value) == "table") and self.context and chunking_on() then
			local s = value
			if type(value) == "table" then
				local ok, r = pcall(self.get_table_save_state, self, value)
				s = ok and r or nil
			end
			if type(s) == "string" and #s > THRESHOLD then
				local n = math.ceil(#s / CHUNK)
				for i = 1, n do
					orig(self, name .. "__se_" .. i, string.sub(s, (i - 1) * CHUNK + 1, i * CHUNK))
				end
				orig(self, name, MARK .. n .. ":" .. #s)
				SV.chunked_saves = SV.chunked_saves + 1
				SV.last_name, SV.last_len = name, #s
				if not SV.logged[name] then
					SV.logged[name] = true
					log("save chunking: " .. name .. " " .. #s .. " bytes saved in " .. n .. " chunks")
				end
				return true
			end
		end
		return orig(self, name, value, ...)
	end
end

-- Join a chunked value from the marker found in its main slot. Only the numbered slots are read
-- here: the main slot has already been read once (by the vanilla function), and a second read of a
-- slot is never needed (0.45.1: the old version read every string / table slot itself before the
-- vanilla read, a second read per value on every load).
local function join_chunks(self, name, raw, default)
	local n, len = string.match(raw, "^%-%-se_chunks:(%d+):(%d+)")
	n, len = tonumber(n), tonumber(len)
	local parts = {}
	for i = 1, (n or 0) do
		local okc, c = pcall(function() return self.context:load_string(name .. "__se_" .. i) end)
		parts[i] = (okc and type(c) == "string") and c or ""
	end
	local s = table.concat(parts)
	if not n or #s ~= len then
		log("save chunking: " .. name .. " is chunked but incomplete (" .. #s .. " of " .. tostring(len) .. " bytes); using the default")
		return default
	end
	SV.chunked_loads = SV.chunked_loads + 1
	SV.last_name, SV.last_len = name, len
	log("save chunking: " .. name .. " " .. len .. " bytes loaded from " .. n .. " chunks")
	if type(default) == "string" then return s end
	local f = loadstring(s)
	if type(f) == "function" then
		local okt, t = pcall(f)
		if okt and type(t) == "table" then return t end
	end
	log("save chunking: " .. name .. " joined but did not convert into a table; using the default")
	return default
end

local function is_marker(v)
	return type(v) == "string" and string.sub(v, 1, #MARK) == MARK
end

local function wrap_load(orig)
	return function(self, name, default, ...)
		local r = orig(self, name, default, ...)
		if type(name) ~= "string" or not self.context then return r end
		if type(default) == "string" then
			-- vanilla hands back the slot's text as it is: a marker means a chunked value
			if is_marker(r) then return join_chunks(self, name, r, default) end
		elseif type(default) == "table" and rawequal(r, default) then
			-- vanilla fell back to the default: the slot may hold a marker (a Lua comment, so it
			-- did not convert into a table). Only this case reads the slot a second time.
			local okr, raw = pcall(function() return self.context:load_string(name) end)
			if okr and is_marker(raw) then return join_chunks(self, name, raw, default) end
		end
		return r
	end
end

-- Wrap one method as it appears; true once both are wrapped.
local function wrap_class(class)
	if rawget(class, "__se_chunking") then return true end
	local s, l = rawget(class, "save_named_value"), rawget(class, "load_named_value")
	if type(s) ~= "function" or type(l) ~= "function" then return false end
	rawset(class, "save_named_value", wrap_save(s))
	rawset(class, "load_named_value", wrap_load(l))
	rawset(class, "__se_chunking", true)
	SV.installed = true
	log("save chunking installed (" .. SV.path .. ")")
	return true
end

-- The class exists but its methods are still being defined: catch their definition.
local function watch_class(class)
	if wrap_class(class) then return end
	if getmetatable(class) ~= nil then
		SV.path = "not installed: the campaign manager class already has a metatable"
		log("save chunking: " .. SV.path)
		return
	end
	local mt = {}
	mt.__newindex = function(t, k, v)
		rawset(t, k, v)
		if (k == "save_named_value" or k == "load_named_value") and wrap_class(t) and getmetatable(t) == mt then
			setmetatable(t, nil)
		end
	end
	setmetatable(class, mt)
end

function se.saves.install()
	if SV.installed then return true, SV.path end
	local class = G("campaign_manager")
	if type(class) == "table" then
		SV.path = "class present at load"
		watch_class(class)
		return SV.installed, SV.path
	end
	-- No class yet: watch the state's globals for its assignment.
	if type(ENV) ~= "table" or rawget(ENV, "__se_chunking_watch") then return false, "waiting for the campaign manager" end
	local prev = getmetatable(ENV)
	local prev_newindex = type(prev) == "table" and prev.__newindex or nil
	local mt = {}
	if type(prev) == "table" then for k, v in pairs(prev) do mt[k] = v end end
	mt.__newindex = function(t, k, v)
		if prev_newindex then
			if type(prev_newindex) == "function" then prev_newindex(t, k, v) else prev_newindex[k] = v end
		else
			rawset(t, k, v)
		end
		if k == "campaign_manager" and type(v) == "table" then
			if getmetatable(t) == mt then setmetatable(t, prev) end
			rawset(t, "__se_chunking_watch", nil)
			SV.path = "class defined after load"
			watch_class(v)
		end
	end
	rawset(ENV, "__se_chunking_watch", true)
	setmetatable(ENV, mt)
	return false, "waiting for the campaign manager"
end

-- se.query.marriage_hook() -> { installed, generations, verdicts, searches, blood_blocked, may_marry } | nil, message
--   DLL 0.42.0-beta.3+: relatives by marriage may marry (script_extender.cfg marriage_inlaws,
--   marriage_blood_generations). verdicts = pair checks the engine made, searches = relatedness
--   searches answered by the DLL, blood_blocked = pairs refused as blood relatives within the
--   configured generations, may_marry = pairs the engine accepted.
function se.query.marriage_hook()
	local okn, err = need("se_marriage_stats")
	if not okn then return nil, err end
	local t = {}
	for k, v in str(se_marriage_stats()):gmatch("([%w_]+)=([^;]*)") do t[k] = tonumber(v) or v end
	t.installed = t.installed == 1
	return t
end

-- se.saves.info() -> { installed, path, chunked_saves, chunked_loads, last_name, last_len, enabled }
function se.saves.info()
	return { installed = SV.installed, path = SV.path, chunked_saves = SV.chunked_saves, chunked_loads = SV.chunked_loads,
		last_name = SV.last_name, last_len = SV.last_len, enabled = chunking_on() }
end

----------------------------------------------------------------------------------------------
-- dilemmas (DLL 0.44+)
----------------------------------------------------------------------------------------------

se.dilemma = se.dilemma or {}

-- se.dilemma.choice(context) -> key, index, order | nil, message
--   Inside a DilemmaChoiceMadeEvent listener only (the engine's event lives on its stack for the
--   duration of the call). context:choice() is the position in the generated dilemma's own
--   choice list, which is not FIRST..FOURTH order; this reads the list itself:
--   key = "FIRST".."FOURTH" of the chosen record, index = context:choice(),
--   order = the keys in the order choice() indexes ({"SECOND","FIRST","FOURTH","THIRD"}).
--   Read only; the same on every client.
local function dilemma_choice(context)
	local okn, err = need("se_dilemma_choice")
	if not okn then return nil, err end
	return G("se_dilemma_choice")(context)
end
se.dilemma.choice = dilemma_choice

-- se.dilemma.choice_key(context) -> "FIRST".."FOURTH" | nil, message
function se.dilemma.choice_key(context)
	local key, idx = dilemma_choice(context)
	if key == nil then return nil, idx end
	return key
end

-- se.dilemma.choice_order(context) -> { keys in choice() order } | nil, message
function se.dilemma.choice_order(context)
	local key, idx, order = dilemma_choice(context)
	if key == nil then return nil, idx end
	return order
end

pcall(se.saves.install)

-- MEDIATE PEACE repair, on by default (0.44+): register se.ui.fix_followup_button() as soon as the
-- game's event manager exists, so no mod has to call it. Off with followup_hooks=0 in
-- script_extender.cfg. CA's lib_header assigns the global `core` only after the libraries are
-- loaded, so when it is not there yet this watches the state's globals for that assignment.
local function auto_followup()
	local function install(core)
		se.core = se.core or core
		local f = G("se_followup_propose")
		if type(f) == "function" and f("installed") then
			local ok, msg = se.ui.fix_followup_button()
			if ok then log("follow-up button fix enabled automatically") elseif msg then log("follow-up button fix not enabled: " .. str(msg)) end
		end
		if type(se.ui.fix_ceo_panel) == "function" then
			local ok, msg = se.ui.fix_ceo_panel()
			log("character panel refresh: " .. str(ok) .. " " .. str(msg))
		end
		-- campaign-AI policies: ai_skills=1 / ai_ceos=1 in script_extender.cfg (default off; in the sync tag)
		local cfg = G("se_ai_cfg")
		if type(cfg) == "function" then
			for _, p in ipairs({ { "ai_skills", se.ai_skills }, { "ai_ceos", se.ai_ceos }, { "ai_court", se.ai_court } }) do
				if cfg(p[1]) and type(p[2]) == "table" then
					local ok, msg = p[2].enable()
					log(p[1] .. " policy enabled from cfg: " .. str(ok) .. " " .. str(msg))
				end
			end
		end
	end
	local f = G("se_followup_propose")
	local c = G("se_ceo_rebuilt")
	if (type(f) ~= "function" or not f("installed")) and type(c) ~= "function" then return end
	local core = G("core")
	if type(core) == "table" and type(core.add_listener) == "function" then return install(core) end
	if type(ENV) ~= "table" or rawget(ENV, "__se_followup_watch") then return end
	local prev = getmetatable(ENV)
	local prev_newindex = type(prev) == "table" and prev.__newindex or nil
	local mt = {}
	if type(prev) == "table" then for k, v in pairs(prev) do mt[k] = v end end
	mt.__newindex = function(t, k, v)
		if prev_newindex then
			if type(prev_newindex) == "function" then prev_newindex(t, k, v) else prev_newindex[k] = v end
		else
			rawset(t, k, v)
		end
		if k == "core" and type(v) == "table" then
			if getmetatable(t) == mt then setmetatable(t, prev) end
			rawset(t, "__se_followup_watch", nil)
			pcall(install, v)
		end
	end
	rawset(ENV, "__se_followup_watch", true)
	setmetatable(ENV, mt)
end
-- se.ui.fix_ceo_panel() -> ok, message   (DLL 0.46.0-beta.3+, on by default; off with
--   ceo_ui_refresh=0). When a character's CEO container is rebuilt (they joined or left a faction)
--   while their character panel is open, every item card in that panel points at freed CEO objects,
--   and a right-click on one (equip / unequip) crashed the game. Every 250 ms this asks the DLL
--   whether such a rebuild happened and, if so, closes the open character panel through its own
--   close button; the next open builds it from the live data.
function se.ui.fix_ceo_panel()
	local okn, err = need("se_ceo_rebuilt")
	if not okn then return false, err end
	local info = str(G("se_ceo_refresh_info") and se_ceo_refresh_info() or "")
	if not string.find(info, "installed=1") then return false, "not installed (ceo_ui_refresh=0 or install failed)" end
	local core = se.core or G("core")
	local rt = G("real_timer")
	if type(core) ~= "table" or type(core.add_listener) ~= "function" then return false, "core (event manager) is not available" end
	if type(rt) ~= "table" or type(rt.register_repeating) ~= "function" then return false, "real_timer is not available" end
	local TIMER = "se_ceo_panel_poll"
	pcall(function() core:remove_listener("se_ceo_panel_poll") end)
	core:add_listener("se_ceo_panel_poll", "RealTimeTrigger",
		function(context) return context.string == TIMER end,
		function()
			local n, cqis = se_ceo_rebuilt()
			if not n or n == 0 then return end
			local root = core.get_ui_root and core:get_ui_root()
			local finder = G("find_uicomponent")
			if not root or type(finder) ~= "function" then return end
			local ok, panel = pcall(finder, root, "character_details")
			if not ok or not panel then return end
			local okv, vis = pcall(function() return panel:Visible() end)
			if not okv or not vis then return end
			local okb, button = pcall(finder, panel, "button_close")
			if okb and button then
				pcall(function() button:SimulateLClick() end)
				log("character panel closed after a CEO rebuild (cqi " .. str(cqis) .. "); reopen it for live data")
			end
		end, true)
	local ok = pcall(rt.register_repeating, TIMER, 250)
	if not ok then ok = pcall(rt.register_repeating, rt, TIMER, 250) end
	if not ok then return false, "could not start the real timer" end
	return true, "polling every 250 ms"
end

-- se.diag.engine_guards() -> table   (DLL 0.46.0-guards.1+, on by default; off with
--   engine_crash_guards=0). Which engine crash guards are installed and how often each one has
--   caught its NULL: { inactive_string = true, inactive_string_hits = 0, ceo_owner = true,
--   ceo_owner_hits = 0, text_font = true, building_browser = true, building_browser_hits = 0 }.
--   text_font has no counter (it is an in-place code patch). Read-only.
function se.diag.engine_guards()
	local okn, err = need("se_engine_guards_info")
	if not okn then return nil, err end
	local t = {}
	for k, v in string.gmatch(str(se_engine_guards_info()), "([%w_]+)=(%d+)") do
		if string.sub(k, -5) == "_hits" then t[k] = tonumber(v) else t[k] = (v == "1") end
	end
	return t
end

-- se.diag.engine_guards_selftest() -> ok, message
--   Calls the two detoured engine functions on objects built inside the DLL (never the game's):
--   InactiveString on an event with no inactive factor group (must not crash or set a string) plus
--   a control with a group (must set it), and the CEO-owner lookup on a NULL owner (must answer
--   false); re-reads the text-font and building-browser code patches. Safe to run at any time.
function se.diag.engine_guards_selftest()
	local okn, err = need("se_engine_guards_selftest")
	if not okn then return false, err end
	return se_engine_guards_selftest()
end

-- se.diag.sepatches() -> table   (DLL 0.46.0-sepatches.1+, on by default)
--   Two engine patches (cfg cai_null_guard, battle_xform_fast; each off with =0):
--   { cai_null_guard = true, cai_sites = 3, cai_hits = 0, battle_xform = true, xform_calls = 0,
--   xform_fallbacks = 0 }. cai_hits counts how often one of the three AI call sites met a missing
--   object; xform_calls counts battle instance-record builds through the faster routine. Read-only.
function se.diag.sepatches()
	local okn, err = need("se_sepatches_info")
	if not okn then return nil, err end
	local t = {}
	for k, v in string.gmatch(str(se_sepatches_info()), "([%w_]+)=(%d+)") do
		if k == "cai_null_guard" or k == "battle_xform" then t[k] = (v == "1") else t[k] = tonumber(v) end
	end
	return t
end

-- se.diag.siege() -> table   (DLL 0.46.0-siege.1+)
--   Counters of the AI assault gate (cfg siege_odds_gate, siege_odds_bands; off with siege_odds_gate=0):
--   { siege_gate = true, calls, vanilla_refusals, refusals, turned_to_accept, bad_frame, last_odds_x100,
--   band1_calls, band1_turned, ... }. calls = rolls the gate compared, vanilla_refusals = rolls the
--   flat 50 % would have refused, refusals = refused with the odds-scaled threshold, turned_to_accept
--   = refused in vanilla but accepted now. bandN = odds band N (band0 = below the first band).
function se.diag.siege()
	local okn, err = need("se_siege_info")
	if not okn then return nil, err end
	local t = {}
	for k, v in string.gmatch(str(se_siege_info()), "([%w_]+)=(%d+)") do
		if k == "siege_gate" or k == "force_assault" then t[k] = (v == "1") else t[k] = tonumber(v) end
	end
	if se.siege and se.siege.stats then
		for k, v in pairs(se.siege.stats) do t["policy_" .. k] = v end
		t.policy_listening = se.siege.listening == true
	end
	return t
end

-- se.diag.sepatches_selftest([n]) -> ok, message
--   Runs n (default 20000, max 2000000) random instance records through the engine's original
--   function and the faster one, and compares every output byte; calls the CAI stub with a NULL
--   and a live object. Uses private buffers only. Safe to run at any time (outside a battle).
function se.diag.sepatches_selftest(n)
	local okn, err = need("se_sepatches_selftest")
	if not okn then return false, err end
	return se_sepatches_selftest(n or 20000)
end

----------------------------------------------------------------------------------------------
-- AI siege assault (DLL 0.47.0-siege+, cfg siege_force_assault, on by default; single player only)
----------------------------------------------------------------------------------------------
-- The campaign AI keeps sitting outside settlements it could take. At every AI faction's turn
-- start each of its besieging armies is checked:
--   * siege age >= se.siege.min_turns (default 1; counted by this module, saved in the game save)
--   * odds = army strength / defenders' strength (garrison + armies in the settlement) >= 1.5
--   * wait chance from the odds (se.siege.bands): 25 % at 1.5-3x, 10 % at 3-6x, 2.5 % at 6x+;
--     the roll is a hash of (turn, army cqi, region key): the same on every machine and reload
--   * walled settlement: raw odds >= 6 (se.siege.walled_min_odds; strength() ignores walls) and
--     assault only with finished equipment (built >= 1, nothing in progress);
--     otherwise a battering ram is queued (force_immediate_construct_siege_item) and it waits
--   * a stance that blocks the attack is changed to DEFAULT first
-- The order goes through se_siege_assault (the turn phase opened for that one attack_settlement
-- call). Multiplayer: not enabled (the phase window is not verified in lockstep).
-- se.diag.siege() carries the counters (fa_* from the DLL, policy_* from here).

se.siege = se.siege or {}
local SG = se.siege
SG.min_turns = SG.min_turns or 1
SG.min_odds = SG.min_odds or 1.5
SG.walled_min_odds = SG.walled_min_odds or 6
SG.bands = SG.bands or { { 1.5, 25 }, { 3, 10 }, { 6, 2.5 } }
SG.stats = SG.stats or { turns = 0, besiegers = 0, young = 0, low_odds = 0, waited = 0, built = 0, need_equipment = 0, stance = 0, ordered = 0, refused = 0, errors = 0 }
SG.last = SG.last or {}

local STANCE_OK = { MILITARY_FORCE_ACTIVE_STANCE_TYPE_DEFAULT = true, MILITARY_FORCE_ACTIVE_STANCE_TYPE_LAND_RAID = true, MILITARY_FORCE_ACTIVE_STANCE_TYPE_SEA_RAID = true }

-- deterministic roll in [0, 100) from (turn, army cqi, region key); integer math stays below 2^24
-- because lua_Number is a 32-bit float in this build
local roll
roll = function(turn, cqi, key)
	local h = 7919
	local s = tostring(turn) .. ":" .. tostring(cqi) .. ":" .. tostring(key)
	for i = 1, #s do
		h = (h * 31 + string.byte(s, i)) % 65521
	end
	h = (h * 241 + 17) % 65521
	return (h % 10000) / 100
end
SG.roll = roll

local function wait_chance(odds)
	local w = nil
	for _, b in ipairs(SG.bands) do
		if odds >= b[1] then w = b[2] end
	end
	return w
end
SG.wait_chance = wait_chance

local function defenders(region)
	local owner = region:owning_faction()
	local total = 0
	local rname = region:name()
	local ml = owner:military_force_list()
	for i = 0, ml:num_items() - 1 do
		local f = ml:item_at(i)
		local ok, inside = pcall(function()
			if f:has_garrison_residence() and f:garrison_residence():region():name() == rname then return true end
			return f:has_general() and f:general_character():is_in_settlement() and f:general_character():region():name() == rname
		end)
		if ok and inside then total = total + f:strength() end
	end
	return total
end

local function age_of(cm, cqi, key, turn)
	local k = "se_siege_age_" .. tostring(cqi)
	local v = cm:get_saved_value(k)
	local vk, vt = nil, nil
	if type(v) == "string" then
		local a, b = string.match(v, "^(.*)|(%d+)$")
		vk, vt = a, tonumber(b)
	end
	if vk ~= key or not vt then
		cm:set_saved_value(k, key .. "|" .. tostring(turn))
		return 0
	end
	return turn - vt
end

local function consider(cm, faction, mf, turn)
	local st = SG.stats
	if not mf:has_general() or mf:is_armed_citizenry() then return end
	local g = mf:general_character()
	if not g:is_besieging() then return end
	st.besiegers = st.besiegers + 1
	local cqi = mf:command_queue_index()
	local region = g:region()
	local key = region:name()
	local rec = { turn = turn, faction = faction:name(), force = cqi, region = key }
	table.insert(SG.last, rec)
	local age = age_of(cm, cqi, key, turn)
	rec.age = age
	if age < SG.min_turns then st.young = st.young + 1 rec.result = "young" return end
	local s = region:settlement()
	local walled = false
	pcall(function() walled = s:has_walls() end)
	rec.walled = walled
	local def = defenders(region)
	local odds = (def > 0) and (mf:strength() / def) or 1000
	rec.odds = math.floor(odds * 100) / 100
	-- raw strength() ignores the walls (the engine's fortification multipliers): a walled target
	-- needs SG.walled_min_odds raw odds (Fang Yue lost Chang'an at 3.8x with full equipment)
	local w = (odds >= (walled and SG.walled_min_odds or SG.min_odds)) and wait_chance(odds) or nil
	if not w then st.low_odds = st.low_odds + 1 rec.result = "low odds" return end
	if walled then
		local built, prog = G("se_siege_equipment")(mf, cqi)
		rec.built, rec.in_progress = built, prog
		if type(built) ~= "number" then st.errors = st.errors + 1 rec.result = "equipment: " .. str(prog) return end
		if built < 1 or prog > 0 then
			if prog == 0 then
				pcall(function() cm:modify_military_force(mf):force_immediate_construct_siege_item("3k_main_blk_battering_ram", true) end)
				st.built = st.built + 1
				local b2, p2 = G("se_siege_equipment")(mf, cqi)
				rec.after_build = str(b2) .. "/" .. str(p2)
			end
			st.need_equipment = st.need_equipment + 1
			rec.result = "building equipment"
			return
		end
	end
	local r = roll(turn, cqi, key)
	rec.roll, rec.wait = r, w
	if r < w then st.waited = st.waited + 1 rec.result = "waits (roll)" return end
	local stance = tostring(mf:active_stance())
	if not STANCE_OK[stance] then
		pcall(function() cm:modify_military_force(mf):change_stance("MILITARY_FORCE_ACTIVE_STANCE_TYPE_DEFAULT") end)
		st.stance = st.stance + 1
		rec.stance = stance
	end
	local ok, msg = G("se_siege_assault")(g, g:command_queue_index(), cm:modify_character(g), s)
	rec.result = (ok and "ASSAULT " or "refused: ") .. str(msg)
	if ok then st.ordered = st.ordered + 1 else st.refused = st.refused + 1 end
	log(string.format("siege: %s force %d at %s age %d odds %.2f walled %s roll %.2f wait %s -> %s", faction:name(), cqi, key, age, odds, str(walled), r, str(w), rec.result))
end

function SG.on_turn_start(faction)
	local cm = cm_()
	if not cm then return end
	SG.stats.turns = SG.stats.turns + 1
	while #SG.last > 60 do table.remove(SG.last, 1) end
	local turn = cm:query_model():turn_number()
	local ml = faction:military_force_list()
	local forces = {}
	for i = 0, ml:num_items() - 1 do table.insert(forces, ml:item_at(i)) end
	for _, mf in ipairs(forces) do
		local okc, err = pcall(consider, cm, faction, mf, turn)
		if not okc then SG.stats.errors = SG.stats.errors + 1 log("siege: error " .. str(err)) end
	end
end

function SG.enable(core)
	core = core or se.core or G("core")
	if type(core) ~= "table" then return false, "core not available" end
	if type(G("se_siege_assault")) ~= "function" then return false, "DLL without se_siege_assault" end
	local info = str(G("se_siege_info")())
	if not string.match(info, "force_assault=1") then return false, "siege_force_assault off in script_extender.cfg (or anchors)" end
	if SG.listening then return true, "already listening" end
	SG.listening = true
	core:add_listener("se_siege_force_assault", "FactionTurnStart", function(context)
		local ok, r = pcall(function()
			local f = context:faction()
			if f:is_human() or f:is_dead() then return false end
			local okm, mp = pcall(function() return cm_():query_model():is_multiplayer() end)
			return okm and mp == false
		end)
		return ok and r
	end, function(context)
		local okp, err = pcall(SG.on_turn_start, context:faction())
		if not okp then SG.stats.errors = SG.stats.errors + 1 log("siege: turn error " .. str(err)) end
	end, true)
	return true, "listening"
end

-- turn the siege assault on as soon as a campaign event manager is known in this state: the
-- chunk's globals rarely hold `core`, but every campaign mod script that uses se sets se.core
local function auto_siege()
	local function go(core)
		local ok, msg = SG.enable(core)
		SG.enable_result = str(ok) .. " " .. str(msg)
		log("siege force assault: " .. SG.enable_result)
	end
	local core = se.core or G("core")
	if type(core) == "table" and type(core.add_listener) == "function" then return go(core) end
	if getmetatable(se) ~= nil then return end
	setmetatable(se, { __newindex = function(t, k, v)
		rawset(t, k, v)
		if k == "core" and type(v) == "table" then
			setmetatable(t, nil)
			pcall(go, v)
		end
	end })
end

pcall(auto_followup)
pcall(auto_siege)

-- ---------------------------------------------------------------------------------------------
-- se.audio: Wwise events on the extender's own game object (DLL with se_audio_* natives; 190E "Soundtrack" jukebox)
--
--   se.audio.available() -> ok, reason
--   se.audio.post(event [, {on_end = fn(info), position = bool}]) -> playing_id | nil, message
--       event = Wwise event name ("Play_Movie_mainmenu_1") or "0x"-hex id string. Never pass a
--       32-bit id as a number: lua_Number is a 32-bit float here and would corrupt it.
--       on_end(info) gets { id, event = "0x..", reason = "end" | "transition" }; "transition" =
--       the track was stopped because a loading screen started (do not auto-advance on it).
--       Game_Mode_* events are refused.
--   se.audio.stop(target [, fade_ms]) / pause / resume -> ok, message
--       target = a playing id from post, or an event name / "0x" id (every instance on our object)
--   se.audio.set_state(group, value) -> ok, message      names or "0x" ids; Game_Mode refused
--   se.audio.position(playing_id) -> ms | nil, message   needs post(..., {position = true}); plain
--                                                        sounds only (music containers have none)
--   se.audio.vanilla_music(on [, fade_ms]) -> ok, message
--       false = pause the game's own dynamic music and keep it paused (the game's resume / re-post
--       is undone by the DLL) until vanilla_music(true) or the next loading screen
--   se.audio.vanilla_music_paused() -> bool
--   se.audio.active_count() -> number of jukebox tracks playing
--   se.audio.on_transition(fn)                           fn() when a loading screen starts (all
--                                                        jukebox tracks are stopped by the DLL)
--   se.audio.poll()                                      drain end notifications now (a real-time
--                                                        timer does it every 250 ms after the
--                                                        first post)
-- Audio is local to this machine and changes nothing in the simulation: safe in multiplayer.
-- ---------------------------------------------------------------------------------------------
se.audio = se.audio or {}
local AU = { on_end = {}, on_transition = {}, timer = false }
se.audio._state = AU
local AU_TIMER = "se_audio_poll"

local function au_info()
	local f = G("se_audio_info")
	if type(f) ~= "function" then return false, "native se_audio_info is not available (DLL without the audio module, or not injected)" end
	return f()
end

function se.audio.available()
	local ok, reason = au_info()
	return ok == true, reason
end

function se.audio.vanilla_music_paused()
	local ok, _, paused = au_info()
	return ok == true and paused == true
end

function se.audio.active_count()
	local ok, _, _, n = au_info()
	if ok ~= true then return 0 end
	return n or 0
end

function se.audio.poll()
	local f = G("se_audio_poll")
	if type(f) ~= "function" then return 0 end
	local items = f()
	if type(items) ~= "table" then return 0 end
	for _, it in ipairs(items) do
		if it.id == 0 then
			for _, fn in ipairs(AU.on_transition) do
				local ok, err = pcall(fn)
				if not ok then log("audio on_transition error: " .. str(err)) end
			end
		else
			local cb = AU.on_end[it.id]
			AU.on_end[it.id] = nil
			if cb then
				local ok, err = pcall(cb, it)
				if not ok then log("audio on_end error: " .. str(err)) end
			end
		end
	end
	return #items
end

-- The engine's real_timer drives RealTimeTrigger in the frontend and the campaign alike
-- (cm:repeat_callback is campaign-only). Started on first use: core does not exist yet when this
-- chunk runs at state creation.
local function au_start_timer()
	if AU.timer then return true end
	local core, rt = G("core"), G("real_timer")
	if type(core) ~= "table" or type(rt) ~= "table" or type(rt.register_repeating) ~= "function" then
		return false
	end
	local ok_l = pcall(function()
		core:add_listener("se_audio_poll_listener", "RealTimeTrigger",
			function(context)
				local okc, s = pcall(function() return context.string end)
				return not okc or s == nil or s == AU_TIMER
			end,
			function() se.audio.poll() end, true)
	end)
	local ok_t = pcall(rt.register_repeating, AU_TIMER, 250)
	if not ok_t then ok_t = pcall(rt.register_repeating, rt, AU_TIMER, 250) end
	AU.timer = ok_l and ok_t
	if not AU.timer then log("audio: could not start the poll timer (listener " .. str(ok_l) .. ", timer " .. str(ok_t) .. "); call se.audio.poll() yourself") end
	return AU.timer
end

local function au_event(ev)
	if type(ev) == "number" then
		if ev >= 16777216 then return nil, "event ids above 2^24 must be passed as \"0x\" strings (lua numbers are 32-bit floats)" end
		return tostring(math.floor(ev))
	end
	if type(ev) ~= "string" or ev == "" then return nil, "event name or \"0x\" id expected" end
	return ev
end

function se.audio.post(ev, opts)
	local okn, err = need("se_audio_post")
	if not okn then return nil, err end
	local e, emsg = au_event(ev)
	if not e then return nil, emsg end
	opts = opts or {}
	local id, msg = se_audio_post(e, opts.position == true)
	if not id then return nil, msg end
	if type(opts.on_end) == "function" then AU.on_end[id] = opts.on_end end
	au_start_timer()
	return id, msg
end

local function au_action(target, action, fade_ms)
	local okn, err = need("se_audio_action")
	if not okn then return false, err end
	if type(target) == "number" then
		return se_audio_action(target, action, fade_ms or 0)
	end
	local e, emsg = au_event(target)
	if not e then return false, emsg end
	return se_audio_action(e, action, fade_ms or 0)
end

function se.audio.stop(target, fade_ms) return au_action(target, 0, fade_ms) end
function se.audio.pause(target, fade_ms) return au_action(target, 1, fade_ms) end
function se.audio.resume(target, fade_ms) return au_action(target, 2, fade_ms) end

function se.audio.set_state(group, value)
	local okn, err = need("se_audio_set_state")
	if not okn then return false, err end
	if type(group) ~= "string" or type(value) ~= "string" then return false, "group and value must be names or \"0x\" strings" end
	return se_audio_set_state(group, value)
end

function se.audio.position(id)
	local okn, err = need("se_audio_position")
	if not okn then return nil, err end
	return se_audio_position(num(id) or 0)
end

function se.audio.vanilla_music(on, fade_ms)
	local okn, err = need("se_audio_vanilla")
	if not okn then return false, err end
	return se_audio_vanilla(on == true, fade_ms or 0)
end

function se.audio.on_transition(fn)
	if type(fn) == "function" then AU.on_transition[#AU.on_transition + 1] = fn end
	au_start_timer()
end

----------------------------------------------------------------------------------------------
-- UI dev loader (DLL 0.47+): open a .twui.xml and its Lua from disk, rebuilt when either file
-- is saved. For testing layouts without a pack. Refused in multiplayer.
----------------------------------------------------------------------------------------------

se.ui = se.ui or {}
local DEV = { handles = {}, order = {}, timer = false }
local DEV_TIMER = "se_ui_dev_watch"

-- The layout's Lua runs in the environment of whoever called se.ui.open (console, mod script):
-- core, cm, find_uicomponent ... live there, not in the raw globals this chunk sees.
local function DG(name, env)
	env = env or DEV.env
	local v = type(env) == "table" and env[name] or nil
	if v == nil then v = G(name) end
	return v
end
local function dev_core(env) return se.core or DG("core", env) end

local function dev_key(path) return string.lower((string.gsub(str(path), "\\", "/"))) end

-- Multiplayer: the attached Lua could change the simulation on this machine only.
local function dev_mp()
	local cm, bm = DG("cm"), DG("bm")
	if type(cm) == "table" then
		local ok, mp = pcall(function() return cm:query_model():is_multiplayer() end)
		if not ok or type(mp) ~= "boolean" then ok, mp = pcall(function() return cm:is_multiplayer() end) end
		if not ok or type(mp) ~= "boolean" then return true end
		return mp
	end
	if type(bm) == "table" then
		local ok, mp = pcall(function() return bm:is_multiplayer() end)
		return ok and mp == true
	end
	return false
end

local function dev_uic(address)
	local UIC = se.UIComponent or DG("UIComponent")
	if type(UIC) ~= "function" and type(UIC) ~= "table" then return nil end
	local ok, u = pcall(UIC, address)
	if ok then return u end
end

-- parent: nil (UI root), a UIComponent object or a component address
local function dev_parent(parent)
	if parent == nil then
		local core = dev_core()
		local ok, root = pcall(function() return core:get_ui_root() end)
		if not ok or not root then return nil, "no UI root in this Lua state" end
		return root
	end
	if type(parent) == "table" or type(parent) == "userdata" then
		local ok, a = pcall(function() return parent:Address() end)
		if ok and a then return a end
	end
	return parent
end

local function dev_close_parts(h)
	if type(h.on_close) == "function" then
		local ok, e = pcall(h.on_close, h)
		if not ok then log("ui dev: on_close of " .. h.twui .. " failed: " .. str(e)) end
	end
	h.on_close = nil
	local core = dev_core(h.env)
	for _, name in ipairs(h.listeners) do
		pcall(function() core:remove_listener(name) end)
	end
	h.listeners = {}
	if h.component then
		local u = dev_uic(h.component)
		if u then pcall(function() u:Destroy() end) end
	end
	h.component = nil
end

-- Run one Lua file with the component; every core:add_listener it makes while it runs is
-- recorded so the next reload can remove it. A returned function becomes on_close.
local function dev_run_lua(h, path, uic)
	local chunk, err = se_uidev_load_lua(path)
	if not chunk then return false, err end
	if type(h.env) == "table" then pcall(setfenv, chunk, h.env) end
	local core = dev_core(h.env)
	local orig = type(core) == "table" and core.add_listener or nil
	-- core is an instance: the method usually lives in its class, so put back exactly what the
	-- instance held before (nil = inherited again)
	local own = orig and rawget(core, "add_listener")
	if orig then
		rawset(core, "add_listener", function(self, name, ...)
			h.listeners[#h.listeners + 1] = name
			return orig(self, name, ...)
		end)
	end
	local ok, r = pcall(chunk, uic, h)
	if orig then rawset(core, "add_listener", own or nil) end
	if not ok then return false, str(r) end
	if type(r) == "function" then h.on_close = r end
	return true
end

local function dev_build(h)
	local virtual, err = se_uidev_place(h.twui)
	if not virtual then return false, err end
	local parent, perr = dev_parent(h.parent)
	if not parent then return false, perr end
	-- the UI root (and a parent handed over as an object) is already a UIComponent
	local pu = parent
	local okm, m = pcall(function() return parent.CreateComponent end)
	if not okm or type(m) ~= "function" then pu = dev_uic(parent) end
	if not pu then return false, "UIComponent is not available in this Lua state" end
	local ok, c = pcall(function() return pu:CreateComponent(h.name, virtual) end)
	if not ok or not c then return false, "CreateComponent(" .. virtual .. ") failed: " .. str(c) end
	h.component = c
	local uic = dev_uic(c)
	for _, p in ipairs(h.lua) do
		local okl, lerr = dev_run_lua(h, p, uic)
		if not okl then return false, p .. ": " .. str(lerr) end
	end
	return true
end

local function dev_stamps(h)
	local t = { se_uidev_stamp(h.twui) or "missing" }
	for _, p in ipairs(h.lua) do t[#t + 1] = se_uidev_stamp(p) or "missing" end
	return table.concat(t, "|")
end

local function dev_reload(h)
	dev_close_parts(h)
	h.stamp = dev_stamps(h)
	local ok, err = dev_build(h)
	h.reloads = h.reloads + 1
	if ok then
		h.error = nil
		log("ui dev: " .. h.twui .. " loaded (#" .. h.reloads .. ", " .. #h.listeners .. " listener(s))")
	else
		h.error = err
		log("ui dev: " .. h.twui .. " FAILED: " .. str(err))
		-- a half-built layout or half-run script must not stay behind
		dev_close_parts(h)
	end
	return ok, err
end

local function dev_poll()
	for _, key in ipairs(DEV.order) do
		local h = DEV.handles[key]
		if h and h.watch then
			local s = dev_stamps(h)
			if s ~= h.stamp then dev_reload(h) end
		end
	end
end

local function dev_start_timer()
	if DEV.timer then return true end
	local core, rt = dev_core(), DG("real_timer")
	if type(core) == "table" and type(rt) == "table" and type(rt.register_repeating) == "function" then
		local ok_l = pcall(function()
			core:add_listener("se_ui_dev_watch_listener", "RealTimeTrigger",
				function(context)
					local okc, s = pcall(function() return context.string end)
					return okc and s == DEV_TIMER
				end,
				function() dev_poll() end, true)
		end)
		local ok_t = pcall(rt.register_repeating, DEV_TIMER, 500)
		if not ok_t then ok_t = pcall(rt.register_repeating, rt, DEV_TIMER, 500) end
		DEV.timer = ok_l and ok_t
	end
	if not DEV.timer then
		local get_tm = DG("get_tm")
		if type(get_tm) == "function" then
			DEV.timer = pcall(function() get_tm():repeat_callback(dev_poll, 500, DEV_TIMER) end)
		end
	end
	if not DEV.timer then log("ui dev: no repeating timer in this Lua state; call se.ui.reload() after saving") end
	return DEV.timer
end

-- se.ui.open(twui_path, opts) -> handle | nil, message
--   opts.lua    = path or { paths }   Lua run after the layout is created, as chunk(component, handle)
--                                      (default: <same folder>/<stem>.lua when it exists)
--   opts.parent = UIComponent or address (default: UI root)
--   opts.name   = component name (default: se_dev_<stem>)
--   opts.watch  = false to turn off the rebuild on save
--   opts.env    = environment for the Lua (default: the caller's, where core / cm live)
-- Opening a path that is already open reloads it with the new options.
function se.ui.open(twui, opts)
	local okn, err = need("se_uidev_place", "se_uidev_stamp", "se_uidev_load_lua")
	if not okn then return nil, err end
	if type(twui) ~= "string" or not string.match(string.lower(twui), "%.twui%.xml$") then
		return nil, "a .twui.xml path is expected"
	end
	if dev_mp() then return nil, "refused in multiplayer: the layout's Lua would run on this machine only" end
	opts = opts or {}
	local okf, caller = pcall(getfenv, 2)
	local env = opts.env or (okf and type(caller) == "table" and caller) or nil
	if env then DEV.env = env end
	local lua = opts.lua
	if type(lua) == "string" then lua = { lua } end
	if lua == nil then
		local sib = string.gsub(twui, "%.[tT][wW][uU][iI]%.[xX][mM][lL]$", ".lua")
		lua = se_uidev_stamp(sib) and { sib } or {}
	end
	local stem = string.match(dev_key(twui), "([^/]+)%.twui%.xml$") or "layout"
	local key = dev_key(twui)
	local h = DEV.handles[key]
	if h then dev_close_parts(h) else
		h = { listeners = {}, reloads = 0 }
		DEV.handles[key] = h
		DEV.order[#DEV.order + 1] = key
	end
	h.twui, h.lua, h.parent, h.env = twui, lua, opts.parent, env or DEV.env
	h.name = opts.name or ("se_dev_" .. stem)
	h.watch = opts.watch ~= false
	local ok, rerr = dev_reload(h)
	if h.watch then dev_start_timer() end
	if not ok then return nil, rerr end
	return h
end

local function dev_find(x)
	if type(x) == "table" and x.twui then return x end
	if type(x) == "string" then return DEV.handles[dev_key(x)] end
end

-- se.ui.reload(handle | path) -> ok, message   (no argument: every open layout)
function se.ui.reload(x)
	if x == nil then
		for _, key in ipairs(DEV.order) do if DEV.handles[key] then dev_reload(DEV.handles[key]) end end
		return true
	end
	local h = dev_find(x)
	if not h then return false, "not open: " .. str(x) end
	return dev_reload(h)
end

-- se.ui.close(handle | path) -> ok   (no argument: every open layout)
function se.ui.close(x)
	local keys = {}
	if x == nil then keys = DEV.order else
		local h = dev_find(x)
		if not h then return false, "not open: " .. str(x) end
		keys = { dev_key(h.twui) }
	end
	for _, key in ipairs(keys) do
		local h = DEV.handles[key]
		if h then dev_close_parts(h); DEV.handles[key] = nil end
	end
	local order = {}
	for _, key in ipairs(DEV.order) do if DEV.handles[key] then order[#order + 1] = key end end
	DEV.order = order
	return true
end

-- se.ui.list() -> { { twui=, lua={..}, component=, reloads=, error= }, ... }
function se.ui.list()
	local t = {}
	for _, key in ipairs(DEV.order) do
		local h = DEV.handles[key]
		t[#t + 1] = { twui = h.twui, lua = h.lua, component = h.component, reloads = h.reloads, error = h.error, watch = h.watch }
	end
	return t
end

log("se_api loaded (dll " .. se.version() .. ")")
