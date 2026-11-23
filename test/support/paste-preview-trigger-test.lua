-- Drive the generated macOS config without a window server or pasteboard.
local handlers, timers = {}, {}
local stages, pastes = 0, 0
package.preload.wezterm = function()
  return {
    on = function(name, handler)
      handlers[name] = handler
    end,
    font = function(name)
      return name
    end,
    action = {
      PasteFrom = function()
        return "paste"
      end,
    },
    action_callback = function(callback)
      return callback
    end,
    time = {
      call_after = function(_, callback)
        table.insert(timers, callback)
      end,
    },
    run_child_process = function(args)
      assert(args[2] == "paste-image" and args[3] == "stage", "unexpected GUI child command")
      stages = stages + 1
      return false, "", "fixture clipboard reader failed"
    end,
  }
end

local vars = {}
local pane = {
  get_user_vars = function()
    return vars
  end,
  get_foreground_process_name = function()
    return "/usr/bin/ssh"
  end,
  pane_id = function()
    return 1
  end,
}
local window = {
  perform_action = function(self, action, target)
    assert(target == pane, "paste targeted a different pane")
    if action == "paste" then
      pastes = pastes + 1
    else
      action(self, target)
    end
  end,
}
dofile(arg[1])
local status = handlers["update-status"]
local trigger = assert(os.getenv("TERMNAV_TEST_PASTE_TRIGGER"))
assert(io.open(trigger, "w")):close()

status(window, pane)
local pending = io.open(trigger, "r")
assert(pending, "the GUI consumed the paste request before it saw tmux ownership")
pending:close()
assert(pastes == 0 and stages == 0 and #timers == 0, "the GUI pasted before tmux was ready")

vars.TERMNAV_TMUX = "true"
status(window, pane)
status(window, pane)
assert(not io.open(trigger, "r"), "the ready GUI did not consume the request")
assert(pastes == 1 and #timers == 1, "the ready GUI did not invoke exactly one real paste action")
timers[1]()
assert(stages == 1, "the ready GUI did not run the clipboard stage once")

local diagnostics = assert(io.open(assert(os.getenv("TERMNAV_TEST_PASTE_STATUS")), "r"))
local log = diagnostics:read("*a")
diagnostics:close()
assert(log:find("waiting for tmux ownership: terminal", 1, true), log)
assert(log:find("consumed trigger: tmux", 1, true), log)
assert(log:find("stage failed: fixture clipboard reader failed", 1, true), log)
print("ok")
