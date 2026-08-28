-- selene: allow(undefined_variable)
local vim = vim

local M = {}

local declined_marker = "__TERMNAV_DECLINED__"
local error_marker = "__TERMNAV_ERROR__"

local pane_directions = {
  left = { window = "h", tmux = "L", edge = "left", key = "<C-h>", move_key = "<M-H>" },
  down = { window = "j", tmux = "D", edge = "bottom", key = "<C-j>", move_key = "<M-J>" },
  up = { window = "k", tmux = "U", edge = "top", key = "<C-k>", move_key = "<M-K>" },
  right = { window = "l", tmux = "R", edge = "right", key = "<C-l>", move_key = "<M-L>" },
}

local function default_command(arguments)
  local output = vim.fn.system(arguments)
  return vim.v.shell_error, output
end

local function default_spawn(arguments, on_exit)
  local stdout = {}
  local job = vim.fn.jobstart(arguments, {
    stdout_buffered = true,
    on_stdout = function(_, data)
      stdout = data or {}
    end,
    on_exit = function(_, status)
      on_exit(status, table.concat(stdout, "\n"):gsub("\n+$", ""))
    end,
  })
  if job <= 0 then
    return nil
  end
  return {
    cancel = function()
      vim.fn.jobstop(job)
    end,
  }
end

local function default_application()
  local function view(window)
    return vim.api.nvim_win_call(window, function()
      return vim.fn.winsaveview()
    end)
  end

  return {
    tab_count = function()
      return #vim.api.nvim_list_tabpages()
    end,
    tab_select = function(direction)
      vim.cmd(direction == "previous" and "tabprevious" or "tabnext")
    end,
    tab_move = function(direction)
      vim.cmd(direction == "left" and "tabmove -1" or "tabmove +1")
    end,
    pane_move = function(source, target)
      local source_buffer = vim.api.nvim_win_get_buf(source)
      local target_buffer = vim.api.nvim_win_get_buf(target)
      local source_view = view(source)
      local target_view = view(target)
      if source_buffer ~= target_buffer then
        local first = pcall(vim.api.nvim_win_set_buf, source, target_buffer)
        local second = first and pcall(vim.api.nvim_win_set_buf, target, source_buffer)
        if not second then
          if first then
            pcall(vim.api.nvim_win_set_buf, source, source_buffer)
          end
          return false
        end
      end
      -- Cursor and viewport are window-local even when both windows display
      -- one buffer. Swap them unconditionally so the user's visible pane,
      -- rather than only its buffer identity, follows the movement.
      pcall(vim.api.nvim_win_call, source, function()
        vim.fn.winrestview(target_view)
      end)
      pcall(vim.api.nvim_win_call, target, function()
        vim.fn.winrestview(source_view)
      end)
      vim.api.nvim_set_current_win(target)
      return true
    end,
  }
end

local function tmux_context()
  local value = vim.env.TMUX or ""
  local socket = value:match("^(.*),%d+,%d+$")
  local pane = vim.env.TMUX_PANE or ""
  if not socket or socket == "" or not pane:match("^%%%d+$") then
    return nil
  end
  return { socket = socket, pane = pane }
end

function M.new(options)
  options = options or {}

  -- Public embedding options: application, command, executable, mappings,
  -- notify, schedule, and spawn. The defaults own native Termnav behavior;
  -- consumers replace a collaborator only when their host API requires it.

  local ctx = {
    application = options.application or default_application(),
    command = options.command or default_command,
    executable = options.executable or "termnav",
    mappings = options.mappings ~= false,
    notify = options.notify or vim.notify,
    schedule = options.schedule or vim.schedule,
    spawn = options.spawn or default_spawn,
  }
  local queue = {}
  local running
  local generation = 0
  local tmux_was_last = false
  local continuation

  local function report(message)
    -- Navigation errors should be visible, but notifications are deliberately
    -- emitted only for launch/queue failures. A command returning "declined"
    -- is ordinary routing policy and stays silent.
    ctx.notify(message, vim.log.levels.WARN, { title = "Termnav" })
  end

  local drain
  drain = function()
    if running ~= nil or #queue == 0 then
      return
    end

    local request = table.remove(queue, 1)
    generation = generation + 1
    local token = { generation = generation }
    running = token

    -- Keep exactly one native process in flight. Serial completion preserves
    -- key order without retaining a resident interpreter, and each successor
    -- observes the focus change completed by its predecessor.
    local arguments = {
      ctx.executable,
      "navigate",
      "--emit-continuation",
      request.action,
      request.direction,
    }
    if continuation ~= nil and continuation ~= "" then
      arguments[#arguments + 1] = "--continuation"
      arguments[#arguments + 1] = continuation
    end
    local ok, handle = pcall(ctx.spawn, arguments, function(status, output)
      ctx.schedule(function()
        -- VimLeave or a replacement generation may invalidate this callback
        -- after the child exits. Never let a stale completion drain a queue it
        -- no longer owns.
        if running ~= token then
          return
        end
        running = nil
        continuation = status == 0 and output ~= nil and output ~= "" and output or nil
        if status ~= 0 and status ~= 3 then
          report("navigation request failed (status " .. tostring(status) .. ")")
        end
        drain()
      end)
    end)
    if not ok or handle == nil or handle == false then
      if running == token then
        running = nil
      end
      report("cannot start native navigation request")
      -- The failed request is consumed, but later queued keys remain useful.
      -- Schedule rather than recurse so a broken executable cannot overflow
      -- Lua's stack while draining a large burst of already queued requests.
      ctx.schedule(drain)
      return
    end
    token.handle = handle
  end

  function ctx.stop()
    queue = {}
    continuation = nil
    generation = generation + 1
    local current = running
    running = nil
    if current ~= nil and type(current.handle) == "table" and current.handle.cancel ~= nil then
      current.handle.cancel()
    end
  end

  function ctx.route(action, direction)
    if #queue >= 100 then
      report("navigation queue is full; dropping the newest request")
      return false
    end
    queue[#queue + 1] = { action = action, direction = direction }
    drain()
    return true
  end

  local function tmux_pane(direction)
    local current = tmux_context()
    if current == nil then
      return "absent"
    end
    local spec = pane_directions[direction]
    local select = string.format("select-pane -t %s -%s", current.pane, spec.tmux)
    local owned = string.format(
      "if-shell -F -t %s '#{!=:#{pane_at_%s},1}' '%s' 'display-message -p %s'",
      current.pane,
      spec.edge,
      select,
      declined_marker
    )
    local status, output = ctx.command({
      "tmux",
      "-S",
      current.socket,
      "if-shell",
      "-F",
      "-t",
      current.pane,
      "#{&&:#{window_active},#{pane_active}}",
      owned,
      "display-message -p " .. error_marker,
    })
    if status ~= 0 or vim.trim(output or "") == error_marker then
      return "error"
    end
    if vim.trim(output or "") == declined_marker then
      return "declined"
    end
    return "handled"
  end

  local function tmux_previous(current)
    ctx.command({
      "tmux",
      "-S",
      current.socket,
      "if-shell",
      "-F",
      "-t",
      current.pane,
      "#{&&:#{window_active},#{pane_active}}",
      "select-pane -t " .. current.pane .. " -l",
      "display-message -p ''",
    })
  end

  function ctx.pane(direction)
    local spec = pane_directions[direction]
    if spec == nil then
      error("invalid pane direction: " .. tostring(direction))
    end

    local before = vim.api.nvim_get_current_win()
    local ok = pcall(vim.cmd, "wincmd " .. spec.window)
    if ok and vim.api.nvim_get_current_win() ~= before then
      tmux_was_last = false
      return true
    end

    if tmux_context() == nil then
      -- Pane selection intentionally stops at the outermost tmux. Termnav
      -- does not assume that a terminal application's panes share tmux's
      -- directional semantics.
      return true
    end

    tmux_was_last = true
    local result = tmux_pane(direction)
    if result == "declined" then
      -- The fast path owns only an adjacent pane in this window. Every
      -- ancestry, client, session, relay, and terminal decision remains in
      -- the shared router.
      return ctx.route("pane-select", direction)
    end
    return true
  end

  function ctx.pane_move(direction)
    local spec = pane_directions[direction]
    if spec == nil then
      error("invalid pane-move direction: " .. tostring(direction))
    end

    local source = vim.api.nvim_get_current_win()
    local ok = pcall(vim.cmd, "wincmd " .. spec.window)
    local neighbor = vim.api.nvim_get_current_win()
    vim.api.nvim_set_current_win(source)
    if ok and neighbor ~= source then
      -- Neovim cannot reparent arbitrary nodes in an asymmetric split tree.
      -- Exchange the two buffers and their views instead: this moves the
      -- user-visible pane one directional step without reshaping unrelated
      -- windows, then follows that content into its new slot.
      if ctx.application.pane_move == nil then
        -- Application collaborators are intentionally partial: consumers may
        -- override only tab behavior. A missing pane primitive therefore
        -- declines locally instead of crashing or moving an outer tmux pane.
        return false
      end
      return ctx.application.pane_move(source, neighbor)
    end

    if tmux_context() == nil then
      return true
    end
    return ctx.route("pane-move", direction)
  end

  function ctx.previous()
    local current = tmux_context()
    if tmux_was_last and current ~= nil then
      tmux_previous(current)
      return true
    end

    local before = vim.api.nvim_get_current_win()
    local ok = pcall(vim.cmd, "wincmd p")
    if ok and vim.api.nvim_get_current_win() ~= before then
      tmux_was_last = false
      return true
    end

    -- Previous-pane toggling is local state, not directional bubbling. Keep
    -- the single tmux operation fast and consume the request at the outermost
    -- scope rather than guessing which ancestor's history the user intended.
    if current ~= nil then
      tmux_previous(current)
      tmux_was_last = true
    end
    return true
  end

  function ctx.application_count()
    local count = ctx.application.tab_count and ctx.application.tab_count() or 1
    return tonumber(count) or 1
  end

  function ctx.tab_select(direction)
    if direction ~= "next" and direction ~= "previous" then
      error("invalid tab selection direction: " .. tostring(direction))
    end
    if ctx.application_count() > 1 then
      if ctx.application.tab_select then
        ctx.application.tab_select(direction)
      end
      return true
    end

    return ctx.route("tab-select", direction)
  end

  function ctx.tab_move(direction)
    if direction ~= "left" and direction ~= "right" then
      error("invalid tab movement direction: " .. tostring(direction))
    end
    if ctx.application_count() > 1 then
      if ctx.application.tab_move then
        ctx.application.tab_move(direction)
      end
      return true
    end

    return ctx.route("tab-move", direction)
  end

  local function keycode(keys)
    -- nvim_replace_termcodes is the stable API across every Neovim release
    -- Termnav supports. Using it directly also avoids a version branch in this
    -- latency-sensitive mapping path.
    return vim.api.nvim_replace_termcodes(keys, true, false, true)
  end

  local function terminal_action(key, callback)
    if vim.bo.filetype == "fzf" then
      return keycode(key)
    end

    -- Terminal expression mappings must leave job mode before executing an
    -- editor command. Restore it only when the selected destination is also a
    -- terminal buffer; normal buffers deliberately remain in normal mode.
    ctx.schedule(function()
      callback()
      if vim.bo.buftype == "terminal" then
        vim.cmd("startinsert")
      end
    end)
    return keycode([[<C-\><C-n>]])
  end

  function ctx.terminal_key(direction)
    local spec = pane_directions[direction]
    if spec == nil then
      error("invalid pane direction: " .. tostring(direction))
    end
    return terminal_action(spec.key, function()
      ctx.pane(direction)
    end)
  end

  function ctx.setup()
    if not ctx.mappings then
      return
    end

    for direction, spec in pairs(pane_directions) do
      local selected_direction = direction
      local move_key = spec.move_key
      local plug = "<Plug>(TermnavPane" .. direction:gsub("^%l", string.upper) .. ")"
      vim.keymap.set("n", plug, function()
        ctx.pane(selected_direction)
      end, { silent = true })
      vim.keymap.set("n", spec.key, function()
        ctx.pane(selected_direction)
      end, { desc = "Navigate " .. selected_direction, silent = true })
      vim.keymap.set("t", spec.key, function()
        return ctx.terminal_key(selected_direction)
      end, { desc = "Navigate " .. selected_direction, expr = true, silent = true })
      vim.keymap.set("n", move_key, function()
        ctx.pane_move(selected_direction)
      end, { desc = "Move pane " .. selected_direction, silent = true })
      vim.keymap.set("t", move_key, function()
        -- Keep terminal mode ownership here rather than making consumers
        -- duplicate the leave/schedule/re-enter sequence. In particular, fzf
        -- must receive the original Meta chord instead of an editor command.
        return terminal_action(move_key, function()
          ctx.pane_move(selected_direction)
        end)
      end, { desc = "Move pane " .. selected_direction, expr = true, silent = true })
    end

    vim.keymap.set("n", "<C-\\>", function()
      ctx.previous()
    end, { desc = "Navigate to previous pane", silent = true })
    local group = vim.api.nvim_create_augroup("TermnavNavigation", { clear = true })
    vim.api.nvim_create_autocmd("WinEnter", {
      group = group,
      callback = function()
        tmux_was_last = false
      end,
    })
    vim.api.nvim_create_autocmd("VimLeavePre", {
      group = group,
      callback = ctx.stop,
    })
    -- Tab navigation is application-level rather than cursor input. Keep it
    -- consistent in normal, insert, and terminal modes so a mode transition
    -- cannot strand the user inside one scope.
    vim.keymap.set({ "n", "i" }, "<C-Tab>", function()
      ctx.tab_select("next")
    end, { desc = "Next application or terminal tab", silent = true })
    vim.keymap.set({ "n", "i" }, "<C-S-Tab>", function()
      ctx.tab_select("previous")
    end, { desc = "Previous application or terminal tab", silent = true })
    vim.keymap.set({ "n", "i" }, "<M-{>", function()
      ctx.tab_move("left")
    end, { desc = "Move application or terminal tab left", silent = true })
    vim.keymap.set({ "n", "i" }, "<M-}>", function()
      ctx.tab_move("right")
    end, { desc = "Move application or terminal tab right", silent = true })
    for key, action in pairs({
      ["<C-Tab>"] = function()
        ctx.tab_select("next")
      end,
      ["<C-S-Tab>"] = function()
        ctx.tab_select("previous")
      end,
      ["<M-{>"] = function()
        ctx.tab_move("left")
      end,
      ["<M-}>"] = function()
        ctx.tab_move("right")
      end,
    }) do
      vim.keymap.set("t", key, function()
        return terminal_action(key, action)
      end, { desc = "Navigate application or terminal tab", expr = true, silent = true })
    end

    if vim.g.Netrw_UserMaps == nil then
      vim.g.Netrw_UserMaps = { { "<C-l>", "<Plug>(TermnavPaneRight)" } }
    end
  end

  return ctx
end

return M
