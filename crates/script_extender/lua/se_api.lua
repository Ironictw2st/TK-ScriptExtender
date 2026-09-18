-- se_api.lua : Lua face of the TW:3K script extender DLL (embedded in the DLL, run once per
-- Lua state right after the se_* natives are registered; a console dofile() of a newer copy
-- simply redefines it).
--
--   se.version()                       DLL version string
--   se.available(native_name)          true when a se_* native exists in this state
--   se.query.*                         read-only, return plain values / tables or nil, message
--   se.modify.*                        mutate; refuse in multiplayer; run on the model thread;
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
	local ok_mp, mp = pcall(function() return cm:is_multiplayer() end)
	if ok_mp and mp == true then return false, "refused: multiplayer campaign" end
	local ok_can, can = pcall(function() return cm:can_modify() end)
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

-- se.query.retinue(cqi) -> { {index, unit_key, strength, experience, can_recruit, is_recruiting, recruiting}, ... }
function se.query.retinue(cqi)
	local q, err = se.character(cqi)
	if not q then return nil, err end
	local list, e2 = slots_of(q)
	if not list then return nil, e2 end
	local out = {}
	for _, e in ipairs(list) do
		local row = { index = e.index, unit_key = e.unit_key }
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
		local okf, fkey = pcall(function() return context:faction():name() end)
		local cm = cm_()
		if not okf or not cm or fkey ~= cm:get_local_faction() then return end
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

-- se.query.autoresolve_prediction() -> { available, attacker_prediction, attacker_casualties,
--   defender_prediction, defender_casualties, night, result_index, results } | nil, message
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

-- plan -> "k=v;..." for the DLL. Values are clamped here (bias 0.1-10, scales and chances 0-1).
local function encode_plan(plan, ctx)
	if type(plan) ~= "table" then return nil, "plan must be a table" end
	local parts = { "v=1" }
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
			rows[#rows + 1] = table.concat({ str(num(d.a)), str(num(d.b)), d.happen == false and "0" or "1",
				str(clamp(d.win_chance, 0, 1, -1)), str(num(d.winner) or -1), d.fate or "-" }, ",")
		end
		if #rows > 0 then put("duels", table.concat(rows, "|")) end
	end
	return table.concat(parts, ";")
end

-- se.modify.autoresolve_plan(plan [, ctx]): store the plan for the current pending battle.
-- (0.24 stores and validates it; the engine hooks that consume it arrive with 0.25 - 0.27.)
function se.modify.autoresolve_plan(plan, ctx)
	local okn, err = need("se_ar_plan_set")
	if not okn then return false, err end
	local cm, e0 = cm_()
	if not cm then return false, e0 end
	local ok_mp, mp = pcall(function() return cm:is_multiplayer() end)
	if ok_mp and mp == true then return false, "refused: multiplayer campaign" end
	ctx = ctx or se.query.pending_battle()
	if not ctx or not ctx.local_player_involved then return false, "refused: no pending battle with the local player" end
	local spec, e1 = encode_plan(plan, ctx)
	if not spec then return false, e1 end
	se._ar_plan = plan
	return se_ar_plan_set(spec)
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
--   Called on every PendingBattle that involves the local player (single-player only), with
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
		if not ctx or not ctx.local_player_involved then return end
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

log("se_api loaded (dll " .. se.version() .. ")")
