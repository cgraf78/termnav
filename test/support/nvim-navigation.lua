-- Headless behavior tests for Termnav's Neovim navigation adapter.
-- selene: allow(undefined_variable)
local vim = vim

local module_path = assert(vim.env.TERMNAV_NAVIGATION_MODULE, "missing navigation module")
local setup_path = assert(vim.env.TERMNAV_SETUP_MODULE, "missing setup module")
local navigation = dofile(module_path)
local passed = 0

local function keycode(keys)
  return vim.api.nvim_replace_termcodes(keys, true, false, true)
end

local function equal(actual, expected, message)
  if not vim.deep_equal(actual, expected) then
    error(message .. ": expected " .. vim.inspect(expected) .. ", got " .. vim.inspect(actual))
  end
end

local function truthy(value, message)
  if not value then
    error(message)
  end
end

local function test(name, callback)
  vim.cmd("silent! only")
  vim.api.nvim_set_current_buf(vim.api.nvim_create_buf(true, false))
  vim.env.TMUX = nil
  vim.env.TMUX_PANE = nil
  vim.bo.filetype = ""
  callback()
  passed = passed + 1
  print("PASS: " .. name)
end

local function fake_context(options)
  options = options or {}
  local commands = {}
  local streams = {}
  options.command = options.command
    or function(arguments)
      commands[#commands + 1] = arguments
      return 0, ""
    end
  options.stream = options.stream
    or function(arguments, on_result, on_exit)
      local stream = {
        arguments = arguments,
        sent = {},
        on_result = on_result,
        on_exit = on_exit,
      }
      function stream.send(request)
        stream.sent[#stream.sent + 1] = request
        return true
      end
      function stream.close()
        stream.closed = true
      end
      streams[#streams + 1] = stream
      return stream
    end
  options.schedule = options.schedule or function(callback)
    callback()
  end
  return navigation.new(options), commands, streams
end

test("local split navigation starts no process", function()
  vim.cmd("vsplit")
  vim.cmd("wincmd h")
  local before = vim.api.nvim_get_current_win()
  local ctx, commands, streams = fake_context()

  equal(ctx.pane("right"), true, "right split should handle navigation")

  truthy(vim.api.nvim_get_current_win() ~= before, "current window should change")
  equal(#commands, 0, "local split should not invoke tmux")
  equal(#streams, 0, "local split should not start the router")
end)

test("adjacent tmux pane stays on the one-process fast path", function()
  vim.env.TMUX = "/tmp/termnav-test.sock,10,0"
  vim.env.TMUX_PANE = "%7"
  local ctx, commands, streams = fake_context()

  equal(ctx.pane("down"), true, "tmux should accept the adjacent pane")

  equal(#commands, 1, "adjacent tmux navigation should use one command")
  truthy(
    table.concat(commands[1], " "):find("pane_active") ~= nil,
    "fast path should guard the source pane"
  )
  truthy(
    table.concat(commands[1], " "):find("window_active") ~= nil,
    "fast path should guard the source window"
  )
  equal(#streams, 0, "adjacent tmux navigation should not start Python")
end)

test("tmux pane edge delegates arbitrary ancestry to the shared router", function()
  vim.env.TMUX = "/tmp/termnav-test.sock,10,0"
  vim.env.TMUX_PANE = "%7"
  local commands = {}
  local ctx, _, streams = fake_context({
    command = function(arguments)
      commands[#commands + 1] = arguments
      return 0, "__TERMNAV_DECLINED__\n"
    end,
  })

  equal(ctx.pane("up"), true, "router should accept the tmux edge")
  equal(#commands, 1, "edge detection should remain one tmux command")
  equal(
    streams[1].arguments,
    { "termnav-navigate", "--stream" },
    "the router should start one ordered stream"
  )
  equal(streams[1].sent, { "pane-select up" }, "the stream should receive the pane request")
end)

test("outermost pane edge does not invent terminal pane navigation", function()
  local ctx, commands, streams = fake_context()

  equal(ctx.pane("left"), true, "outermost edge should be consumed")
  equal(#commands, 0, "no tmux means no local probe")
  equal(#streams, 0, "pane navigation should not target terminal panes implicitly")
end)

test("application tab selection stays process free", function()
  local selected = {}
  local ctx, commands, streams = fake_context({
    application = {
      tab_count = function()
        return 2
      end,
      tab_select = function(direction)
        selected[#selected + 1] = direction
      end,
    },
  })

  equal(ctx.tab_select("next"), true, "application should own multiple tabs")

  equal(selected, { "next" }, "application callback should receive direction")
  equal(#commands, 0, "application tab should not invoke tmux")
  equal(#streams, 0, "application tab should not start the router")
end)

test("application tab movement owns its boundary no-op", function()
  local moved = {}
  local ctx, commands, streams = fake_context({
    application = {
      tab_count = function()
        return 2
      end,
      tab_move = function(direction)
        moved[#moved + 1] = direction
      end,
    },
  })

  equal(ctx.tab_move("left"), true, "application should own movement with multiple tabs")

  equal(moved, { "left" }, "application should receive the movement request")
  equal(#commands, 0, "application movement should not invoke tmux")
  equal(#streams, 0, "application boundary should not bubble")
end)

test("single application tab delegates linked-session policy to the router", function()
  vim.env.TMUX = "/tmp/termnav-test.sock,10,0"
  vim.env.TMUX_PANE = "%7"
  local ctx, commands, streams = fake_context({
    application = {
      tab_count = function()
        return 1
      end,
    },
  })

  equal(ctx.tab_select("previous"), true, "router should own the tmux boundary")

  equal(#commands, 0, "nvim should not choose a tmux session itself")
  equal(
    streams[1].arguments,
    { "termnav-navigate", "--stream" },
    "tab selection should start one ordered stream"
  )
  equal(streams[1].sent, { "tab-select previous" }, "the stream should receive the tab request")
end)

test("boundary bursts share one ordered router stream", function()
  vim.env.TMUX = "/tmp/termnav-test.sock,10,0"
  vim.env.TMUX_PANE = "%7"
  local ctx, _, streams = fake_context({
    command = function()
      return 0, "__TERMNAV_DECLINED__"
    end,
  })

  ctx.pane("left")
  ctx.pane("right")
  equal(#streams, 1, "one worker should own the entire burst")
  equal(streams[1].arguments, { "termnav-navigate", "--stream" }, "stream command")
  equal(
    streams[1].sent,
    { "pane-select left", "pane-select right" },
    "requests should preserve input order"
  )
end)

test("setup prewarms one persistent router stream", function()
  local ctx, _, streams = fake_context({ mappings = true })

  ctx.setup()

  equal(#streams, 1, "setup should hide Python startup before the first gesture")
  equal(streams[1].arguments, { "termnav-navigate", "--stream" }, "prewarmed stream command")
  equal(streams[1].sent, {}, "prewarming should not invent a navigation request")
  truthy(not streams[1].closed, "the shared worker should remain ready between gestures")
end)

test("setup remains usable while the router executable is unavailable", function()
  local ctx = navigation.new({
    mappings = true,
    stream = function()
      error("router unavailable")
    end,
  })

  local ok = pcall(ctx.setup)
  truthy(ok, "an unavailable prewarm must not abort editor setup")
end)

test("previous split navigation remains local and process free", function()
  vim.cmd("vsplit")
  vim.cmd("wincmd l")
  local before = vim.api.nvim_get_current_win()
  vim.cmd("wincmd h")
  local expected = vim.api.nvim_get_current_win()
  vim.api.nvim_set_current_win(before)
  local ctx, commands, streams = fake_context()

  equal(ctx.previous(), true, "previous split should handle navigation")

  equal(vim.api.nvim_get_current_win(), expected, "previous split should be restored")
  equal(#commands, 0, "previous split should not invoke tmux")
  equal(#streams, 0, "previous split should not start the router")
end)

test("single nvim split delegates previous-pane history only to its immediate tmux", function()
  vim.env.TMUX = "/tmp/termnav-test.sock,10,0"
  vim.env.TMUX_PANE = "%7"
  local ctx, commands, streams = fake_context()

  equal(ctx.previous(), true, "tmux should own previous-pane history")

  equal(#commands, 1, "previous pane should use one guarded tmux command")
  truthy(
    table.concat(commands[1], " "):find("select%-pane %-t %%7 %-l") ~= nil,
    "previous pane should target the immediate tmux pane"
  )
  equal(#streams, 0, "previous pane should not enter the boundary router")
end)

test("previous pane without a tmux scope is a local no-op", function()
  local ctx, commands, streams = fake_context()

  equal(ctx.previous(), true, "outermost previous pane should be consumed")

  equal(#commands, 0, "outermost previous pane should not invoke tmux")
  equal(#streams, 0, "outermost previous pane should not invent a terminal route")
end)

test("previous pane returns to tmux after crossing the nvim boundary", function()
  vim.env.TMUX = "/tmp/termnav-test.sock,10,0"
  vim.env.TMUX_PANE = "%7"
  vim.cmd("vsplit")
  vim.cmd("wincmd h")
  local source = vim.api.nvim_get_current_win()
  local commands = {}
  local ctx, _, streams = fake_context({
    command = function(arguments)
      commands[#commands + 1] = arguments
      return 0, "__TERMNAV_DECLINED__"
    end,
  })

  ctx.pane("left")
  equal(streams[1].sent, { "pane-select left" }, "boundary route")
  ctx.previous()

  equal(vim.api.nvim_get_current_win(), source, "previous should not enter another nvim split")
  equal(#commands, 2, "edge detection and previous should each use one tmux command")
  truthy(
    table.concat(commands[2], " "):find("select%-pane") ~= nil,
    "previous should select the last tmux pane"
  )
  truthy(
    table.concat(commands[2], " "):find("window_active") ~= nil,
    "previous should guard the source window"
  )
end)

test("fzf terminal navigation returns the original control key", function()
  local ctx, commands, streams = fake_context()
  vim.bo.filetype = "fzf"

  equal(ctx.terminal_key("left"), keycode("<C-h>"), "fzf should receive raw C-h")

  equal(#commands, 0, "fzf passthrough should not invoke tmux")
  equal(#streams, 0, "fzf passthrough should not start the router")
end)

test("terminal navigation restores terminal-job mode", function()
  if vim.fn.executable("sh") ~= 1 then
    return
  end
  local restored = 0
  local ctx = fake_context()
  vim.api.nvim_set_current_buf(vim.api.nvim_create_buf(true, false))
  vim.fn.termopen({ "sh", "-c", "exit 0" })
  equal(vim.bo.buftype, "terminal", "test should use a real terminal buffer")
  local command = vim.cmd
  vim.cmd = function(value)
    if value == "startinsert" then
      restored = restored + 1
      return
    end
    return command(value)
  end

  equal(ctx.terminal_key("left"), keycode([[<C-\><C-n>]]), "terminal escape")
  vim.cmd = command
  equal(restored, 1, "navigation should restore terminal-job mode")
end)

test("setup installs direct mappings and netrw routing", function()
  vim.g.Netrw_UserMaps = nil
  local ctx = fake_context({ mappings = true })

  ctx.setup()

  -- Keep the complete vim-tmux-navigator migration surface explicit. A future
  -- refactor must not preserve only the four directional mappings while
  -- silently dropping the fifth previous-pane operation again.
  for _, expected in ipairs({
    { key = "<C-h>", desc = "Navigate left" },
    { key = "<C-j>", desc = "Navigate down" },
    { key = "<C-k>", desc = "Navigate up" },
    { key = "<C-l>", desc = "Navigate right" },
    { key = "<C-\\>", desc = "Navigate to previous pane" },
  }) do
    local mapping = vim.fn.maparg(expected.key, "n", false, true)
    truthy(mapping.callback ~= nil, expected.key .. " should map a navigation callback")
    equal(mapping.desc, expected.desc, expected.key .. " mapping description")
  end
  equal(vim.fn.maparg("<C-j>", "t", false, true).expr, 1, "terminal pane map should be expr")
  equal(
    vim.fn.maparg("<C-\\>", "t"),
    "",
    "terminal Ctrl-backslash must remain available for the terminal-mode escape prefix"
  )
  for _, mode in ipairs({ "n", "i", "t" }) do
    truthy(
      vim.fn.maparg("<C-Tab>", mode, false, true).callback ~= nil,
      "Ctrl-Tab should route in mode " .. mode
    )
    truthy(
      vim.fn.maparg("<M-{>", mode, false, true).callback ~= nil,
      "tab movement should route in mode " .. mode
    )
  end
  vim.bo.filetype = "fzf"
  for _, key in ipairs({ "<C-Tab>", "<C-S-Tab>", "<M-{>", "<M-}>" }) do
    local mapping = vim.fn.maparg(key, "t", false, true)
    equal(mapping.callback(), keycode(key), "fzf should receive raw " .. key)
  end
  equal(vim.g.Netrw_UserMaps[1][1], "<C-l>", "netrw should preserve right navigation")
  equal(
    vim.g.Netrw_UserMaps[1][2],
    "<Plug>(TermnavPaneRight)",
    "netrw should reuse Termnav's direct mapping"
  )
end)

test("Termnav setup installs the navigation adapter once", function()
  local setup = dofile(setup_path)
  local calls = 0
  local adapter = {
    setup = function()
      calls = calls + 1
    end,
  }
  local ctx = setup.new({
    navigation = adapter,
    opener = { setup = function() end },
    wezterm_vars = {
      set = function()
        return true
      end,
    },
    vscode_focus = {
      focus = function() end,
      blur = function() end,
      dispose = function() end,
    },
    publish_delay_ms = -1,
  })

  ctx.setup()

  equal(calls, 1, "shared setup should install navigation")
  equal(ctx.navigation, adapter, "context should expose the installed adapter")
end)

print(string.format("nvim-navigation: %d passed", passed))
