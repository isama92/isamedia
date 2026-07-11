-- skip_button.lua
-- Netflix-style contextual "Skip Intro/Outro" button for chaptered files.
--
-- Author: Stefano Borzoni <isama92@gmail.com>
-- Part of isamedia (https://github.com/isama92/isamedia); GPL-3.0-only.
--
-- Shows a clickable button near the bottom-right of the window while playback
-- is inside a chapter whose title matches the intro/outro patterns. Clicking it
-- (or pressing the trigger key) seeks to the end of that chapter (the start of
-- the next chapter, or the end of the file if it is the last one). It never
-- advances to the next playlist item.
--
-- Designed to coexist with the ModernZ OSC: it only grabs a left-click while the
-- cursor is actually over the button, so normal clicks reach ModernZ untouched.

local options = require "mp.options"

local opts = {
    intro_patterns = "opening;intro;op",             -- ; separated, case-insensitive substrings
    outro_patterns = "ending;credit;outro;preview;ed",
    exclude_patterns = "credits end;post-credit;post credit",  -- never show for these (e.g. post-credits scenes)
    bottom_margin = 200,   -- pixels between the button's bottom edge and the window bottom
    right_margin = 60,     -- pixels between the button's right edge and the window right
    key = "ENTER",         -- key that triggers the button while it is visible (Space stays free to pause the intro/outro)
}
options.read_options(opts, "skip_button")

local box = mp.create_osd_overlay("ass-events")
local label = mp.create_osd_overlay("ass-events")

local current = nil     -- { label = string, target = number (seconds), rect = {...} }
local hovering = false
local click_bound = false
local key_bound = false

local draw, clear_button, do_skip

local function split(s)
    local t = {}
    for p in s:gmatch("[^;]+") do
        p = p:gsub("^%s+", ""):gsub("%s+$", ""):lower()
        if p ~= "" then t[#t + 1] = p end
    end
    return t
end
local intro_pats = split(opts.intro_patterns)
local outro_pats = split(opts.outro_patterns)
local exclude_pats = split(opts.exclude_patterns)

local function match_any(title, pats)
    if not title then return false end
    title = title:lower()
    for _, p in ipairs(pats) do
        if title:find(p, 1, true) then return true end
    end
    return false
end

local function category(title)
    if match_any(title, exclude_pats) then return nil end
    if match_any(title, intro_pats) then return "intro" end
    if match_any(title, outro_pats) then return "outro" end
    return nil
end

draw = function()
    if not current then
        box:remove()
        label:remove()
        return
    end
    local w = mp.get_property_number("osd-width", 0)
    local h = mp.get_property_number("osd-height", 0)
    if w == 0 or h == 0 then return end

    local bw = 60 + #current.label * 15
    local bh = 52
    local x2 = w - opts.right_margin
    local y2 = h - opts.bottom_margin
    local x1 = x2 - bw
    local y1 = y2 - bh
    current.rect = { x1 = x1, y1 = y1, x2 = x2, y2 = y2 }

    local bg = hovering and "454545" or "202020"
    local bga = hovering and "10" or "30"

    box.res_x, box.res_y = w, h
    box.data = string.format(
        "{\\an7\\pos(%d,%d)\\bord2\\3c&HFFFFFF&\\3a&H50&\\shad0\\1c&H%s&\\1a&H%s&\\p1}m 0 0 l %d 0 l %d %d l 0 %d{\\p0}",
        x1, y1, bg, bga, bw, bw, bh, bh)
    box:update()

    label.res_x, label.res_y = w, h
    label.data = string.format(
        "{\\an5\\pos(%d,%d)\\fs28\\bord1\\3c&H000000&\\shad1\\1c&HFFFFFF&}%s",
        (x1 + x2) / 2, (y1 + y2) / 2, current.label)
    label:update()
end

local function bind_click()
    if not click_bound then
        mp.add_forced_key_binding("mbtn_left", "skip_button_click", do_skip)
        click_bound = true
    end
end

local function unbind_click()
    if click_bound then
        mp.remove_key_binding("skip_button_click")
        click_bound = false
    end
end

clear_button = function()
    current = nil
    hovering = false
    unbind_click()
    if key_bound then
        mp.remove_key_binding("skip_button_key")
        key_bound = false
    end
    box:remove()
    label:remove()
end

do_skip = function()
    if not current then return end
    local target = current.target
    clear_button()
    mp.commandv("seek", tostring(target), "absolute+exact")
end

local function update_chapter()
    local idx = mp.get_property_number("chapter", -1)
    local list = mp.get_property_native("chapter-list", {})
    if not idx or idx < 0 or not list[idx + 1] then
        clear_button()
        return
    end
    if not category(list[idx + 1].title) then
        clear_button()
        return
    end

    -- target: the end of the current chapter, i.e. the start of the next chapter,
    -- or the end of the file if this is the last chapter. Never advances the playlist.
    local target = (list[idx + 2] and list[idx + 2].time) or mp.get_property_number("duration")
    if not target then
        clear_button()
        return
    end

    current = {
        label = "Skip",
        target = target,
    }
    hovering = false
    unbind_click()
    if not key_bound then
        mp.add_forced_key_binding(opts.key, "skip_button_key", do_skip)
        key_bound = true
    end
    draw()
end

mp.observe_property("chapter", "number", update_chapter)
mp.observe_property("chapter-list", "native", update_chapter)
mp.observe_property("osd-width", "number", function() if current then draw() end end)
mp.observe_property("osd-height", "number", function() if current then draw() end end)

mp.observe_property("mouse-pos", "native", function(_, pos)
    if not current or not current.rect then return end
    local r = current.rect
    local inside = pos and pos.x ~= nil
        and pos.x >= r.x1 and pos.x <= r.x2
        and pos.y >= r.y1 and pos.y <= r.y2
    if inside and not hovering then
        hovering = true
        bind_click()
        draw()
    elseif not inside and hovering then
        hovering = false
        unbind_click()
        draw()
    end
end)
