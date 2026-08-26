-- selene: allow(undefined_variable)
local vim = vim

local M = {}

local declined_marker = "__TERMNAV_DECLINED__"
local error_marker = "__TERMNAV_ERROR__"

local pane_directions = {
  left = { window = "h", tmux = "L", edge = "left", key = "<C-h>" },
  down = { window = "j", tmux = "D", edge = "bottom", key = "<C-j>" },
  up = { window = "k", tmux = "U", edge = "top", key = "<C-k>" },
  right = { window = "l", tmux = "R", edge = "right", key = "<C-l>" },
}

local function default_command(arguments)
  local output = vim.fn.system(arguments)
  return vim.v.shell_error, output
end

local function default_stream(arguments, on_result, on_exit)
  local partial = ""
  local job = vim.fn.jobstart(arguments, {
    stdout_buffered = false,
    on_stdout = function(_, data)
      for index, chunk in ipairs(data) do
        partial = partial .. chunk
        if index < #data then
          if partial ~= "" then
            on_result(tonumber(partial))
          end
          partial = ""
        end
      end
    end,
    on_exit = function(_, status)
      on_exit(status)
    end,
  })
  if job <= 0 then
    return nil
  end
  return {
    send = function(request)
      return vim.fn.chansend(job, request .. "\n") > 0
    end,
    close = function()
      vim.fn.chanclose(job, "stdin")
    end,
  }
end

local function default_application()
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

  local ctx = {
    application = options.application or default_application(),
    command = options.command or default_command,
    defer = options.defer or vim.defer_fn,
    executable = options.executable or "termnav-navigate",
    mappings = options.mappings ~= false,
    schedule = options.schedule or vim.schedule,
    stream = options.stream or default_stream,
    stream_idle_ms = options.stream_idle_ms or 250,
  }
  local worker
  local outstanding = 0
  local generation = 0
  local tmux_was_last = false

  local function retire_when_idle(expected_generation)
    ctx.defer(function()
      if worker ~= nil and outstanding == 0 and generation == expected_generation then
        local retiring = worker
        worker = nil
        retiring.close()
      end
    end, ctx.stream_idle_ms)
  end

  function ctx.route(action, direction)
    generation = generation + 1
    if worker == nil then
      local created
      created = ctx.stream({ ctx.executable, "--stream" }, function()
        ctx.schedule(function()
          if worker ~= created then
            return
          end
          outstanding = math.max(0, outstanding - 1)
          if outstanding == 0 then
            retire_when_idle(generation)
          end
        end)
      end, function()
        ctx.schedule(function()
          if worker == created then
            worker = nil
            outstanding = 0
          end
        end)
      end)
      worker = created
    end
    if worker ~= nil then
      outstanding = outstanding + 1
      if not worker.send(action .. " " .. direction) then
        worker.close()
        worker = nil
        outstanding = 0
      end
    end
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
