-- Open file paths clicked in tmux/WezTerm panes inside this nvim instance.
--
-- This module intentionally resolves paths against terminal state (the clicked
-- pane cwd, terminal job cwd, and literal $HOME references), not workspace repo
-- roots. A terminal link like `src/main.lua` should mean "relative to where
-- that command printed it", even when nvim's active buffer belongs to another
-- repo or the bare home workspace.

-- selene: allow(undefined_variable)
local vim = vim
-- selene: allow(global_usage)
local _G = _G

local M = {}

local function state_home()
  local path = vim.env.XDG_STATE_HOME
  if type(path) == "string" and path:match("^/") then
    return path
  end

  local home = vim.env.HOME
  if type(home) == "string" and home ~= "" then
    return home .. "/.local/state"
  end

  return nil
end

local state_root = state_home()
local state_dir = state_root and (state_root .. "/nvim-tmux-open") or nil
local server_address

local function pane_key()
  local pane = vim.env.TMUX_PANE
  local tmux = vim.env.TMUX
  local socket = tmux and tmux:match("^[^,]+")
  if pane and pane ~= "" then
    local key = pane
    if socket and socket ~= "" then
      key = socket .. ":" .. pane
    end
    return key:gsub("[^%w_.-]", "_")
  end
  return tostring(vim.fn.getpid())
end

local function write(path, value)
  vim.fn.mkdir(vim.fn.fnamemodify(path, ":h"), "p")
  vim.fn.writefile({ value }, path)
end

function M.server()
  -- Keep one advertised address even if this instance had to start its own
  -- RPC socket; v:servername is not guaranteed to change after serverstart().
  if server_address and server_address ~= "" then
    return server_address
  end

  if vim.v.servername ~= "" then
    server_address = vim.v.servername
  else
    if not state_dir then
      return ""
    end
    local socket = state_dir .. "/nvim-" .. pane_key() .. ".sock"
    vim.fn.delete(socket)
    server_address = vim.fn.serverstart(socket)
  end

  return server_address
end

local function normal_win()
  local alt = vim.fn.win_getid(vim.fn.winnr("#"))
  if alt ~= 0 and vim.api.nvim_win_is_valid(alt) then
    local buf = vim.api.nvim_win_get_buf(alt)
    if vim.bo[buf].buftype == "" then
      return alt
    end
  end

  local current = vim.api.nvim_get_current_win()
  local current_buf = vim.api.nvim_win_get_buf(current)
  if vim.bo[current_buf].buftype == "" then
    return current
  end

  for _, win in ipairs(vim.api.nvim_tabpage_list_wins(0)) do
    local buf = vim.api.nvim_win_get_buf(win)
    if vim.bo[buf].buftype == "" then
      return win
    end
  end

  return current
end

local function absolute(path)
  return path:match("^/") or path:match("^%a:[/\\]")
end

local function join(dir, path)
  return (dir:gsub("/$", "")) .. "/" .. path
end

local function expand_home_ref(path)
  local home = vim.env.HOME
  if type(home) ~= "string" or home == "" then
    return path
  end

  -- Ctrl-click regex links keep source snippets literal. Expand only the
  -- conventional home aliases, not general shell syntax.
  if path == "$HOME" or path == "${HOME}" then
    return home
  end

  local suffix = path:match("^%$HOME/(.+)$")
  if suffix then
    return join(home, suffix)
  end

  suffix = path:match("^%${HOME}/(.+)$")
  if suffix then
    return join(home, suffix)
  end

  return path
end

local function exists(path)
  return vim.fn.filereadable(path) == 1 or vim.fn.isdirectory(path) == 1
end

local function realpath(path)
  if type(path) ~= "string" or path == "" then
    return path
  end

  local uv = vim.uv or vim.loop
  if uv and uv.fs_realpath then
    local ok, resolved = pcall(uv.fs_realpath, path)
    if ok and resolved and resolved ~= "" then
      return resolved
    end
  end

  return path
end

local function proc_cwd(pid)
  -- Linux exposes process cwd as a /proc symlink. Prefer it when available so
  -- WSL/minimal hosts do not need lsof just to resolve terminal click paths.
  local cwd = realpath("/proc/" .. tostring(pid) .. "/cwd")
  if cwd ~= "/proc/" .. tostring(pid) .. "/cwd" then
    return cwd
  end

  return nil
end

function M.terminal_cwd(win)
  win = win or vim.api.nvim_get_current_win()
  if not vim.api.nvim_win_is_valid(win) then
    return nil
  end

  local buf = vim.api.nvim_win_get_buf(win)
  local job = vim.b[buf].terminal_job_id
  if not job then
    return nil
  end

  local pid = vim.fn.jobpid(job)
  if not pid or pid <= 0 then
    return nil
  end

  local process_cwd = proc_cwd(pid)
  if process_cwd then
    return process_cwd
  end

  if vim.fn.executable("lsof") ~= 1 then
    return nil
  end

  local lines = vim.fn.systemlist({ "lsof", "-a", "-p", tostring(pid), "-d", "cwd", "-Fn" })
  if vim.v.shell_error ~= 0 then
    return nil
  end

  for _, line in ipairs(lines) do
    local cwd = line:match("^n(.+)$")
    if cwd then
      return realpath(cwd)
    end
  end

  return nil
end

local function resolve(path, base_cwd, source)
  if source ~= "cli" then
    path = expand_home_ref(path)
  end

  if absolute(path) then
    return path
  end

  local current = vim.api.nvim_get_current_win()
  local base = type(base_cwd) == "string" and base_cwd ~= "" and base_cwd or nil
  local term = M.terminal_cwd(current)
  local from_nvim = source == "nvim"

  if base then
    local base_path = join(base, path)
    if exists(base_path) and not from_nvim then
      return base_path
    end
  end
  if term and from_nvim then
    local term_path = join(term, path)
    if exists(term_path) then
      return term_path
    end
  end
  if base then
    return join(base, path)
  end

  return path
end

function M.open_now(path, line, col, base_cwd, source)
  if type(path) ~= "string" or path == "" then
    return false
  end

  path = resolve(path, base_cwd, source)
  line = tonumber(line) or 1
  col = tonumber(col) or 0
  if line < 1 then
    line = 1
  end

  local win = normal_win()
  if vim.api.nvim_win_is_valid(win) then
    vim.api.nvim_set_current_win(win)
  end

  vim.cmd("edit +" .. line .. " " .. vim.fn.fnameescape(path))
  if col > 0 then
    pcall(vim.api.nvim_win_set_cursor, 0, { line, col - 1 })
  end

  return true
end

function M.open(path, line, col, base_cwd, source)
  vim.schedule(function()
    local ok, err = pcall(M.open_now, path, line, col, base_cwd, source)
    if not ok then
      vim.notify("nvim-tmux-open: " .. tostring(err), vim.log.levels.ERROR)
    end
  end)
  return true
end

function M.setup()
  _G.nvim_tmux_open = M.open
  if not state_dir then
    return false
  end

  vim.fn.mkdir(state_dir, "p")

  local function publish()
    local address = M.server()
    write(state_dir .. "/panes/" .. pane_key(), address)
    write(state_dir .. "/current", address)
  end

  publish()

  local group = vim.api.nvim_create_augroup("nvim_tmux_open", { clear = true })
  vim.api.nvim_create_autocmd({
    "BufEnter",
    "FocusGained",
    "TermEnter",
    "VimEnter",
    "WinEnter",
  }, {
    group = group,
    callback = publish,
  })

  vim.api.nvim_create_autocmd("VimLeavePre", {
    group = group,
    callback = function()
      vim.fn.delete(state_dir .. "/panes/" .. pane_key())
      if vim.fn.filereadable(state_dir .. "/current") == 1 then
        local current = vim.fn.readfile(state_dir .. "/current")[1]
        if current == M.server() then
          vim.fn.delete(state_dir .. "/current")
        end
      end
    end,
  })

  return true
end

return M
