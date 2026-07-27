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
local owned_socket
local uv = vim.uv or vim.loop
local publish_counter = 0

local function user_id()
  if type(uv.os_get_passwd) == "function" then
    local passwd = uv.os_get_passwd()
    if type(passwd) == "table" and passwd.uid ~= nil then
      return passwd.uid
    end
  end
  return vim.fn.getuid()
end

local function hex_encode(value)
  return (value:gsub(".", function(char)
    return string.format("%02x", string.byte(char))
  end))
end

local function registry_key(value)
  local encoded = hex_encode(value)
  local components = {}
  local offset = 1
  while offset <= #encoded do
    local prefix = offset == 1 and "v1-" or ""
    components[#components + 1] = prefix .. encoded:sub(offset, offset + 119)
    offset = offset + 120
  end
  return table.concat(components, "/")
end

local function tmux_identity()
  local tmux = vim.env.TMUX
  if type(tmux) ~= "string" then
    return "", ""
  end

  local socket, server_pid = tmux:match("^(.*),([^,]*),[^,]*$")
  if not socket then
    return tmux, ""
  end
  return socket, server_pid
end

local function pane_key()
  local pane = vim.env.TMUX_PANE
  if pane and pane ~= "" then
    local socket, server_pid = tmux_identity()
    return registry_key(socket .. "\0" .. server_pid .. "\0" .. pane)
  end
  return registry_key("\0\0" .. tostring(vim.fn.getpid()))
end

local process_pid = tostring(vim.fn.getpid())
local process_nonce = tostring(uv.hrtime()):gsub("[^%w_.-]", "_")
local owner_key = "p" .. process_pid .. "-" .. process_nonce
local socket_key = process_pid .. "-" .. process_nonce

local function ensure_private_dir(path)
  if vim.fn.mkdir(path, "p", 448) == -1 and vim.fn.isdirectory(path) ~= 1 then
    error("cannot create private registry directory: " .. path)
  end
  if vim.fn.setfperm(path, "rwx------") ~= 1 then
    error("cannot secure registry directory: " .. path)
  end
end

local function ensure_private_runtime_dir(path)
  if vim.fn.mkdir(path, "p", 448) == -1 then
    error("cannot create private socket directory: " .. path)
  end
  local stat, stat_error = uv.fs_lstat(path)
  if not stat or stat.type ~= "directory" then
    local detail = stat_error or (stat and stat.type) or "unknown file type"
    error("unsafe socket directory: " .. path .. ": " .. tostring(detail))
  end
  local uid = user_id()
  if stat.uid and uid >= 0 and stat.uid ~= uid then
    error("socket directory is owned by another user: " .. path)
  end
  if vim.fn.setfperm(path, "rwx------") ~= 1 then
    error("cannot secure socket directory: " .. path)
  end
end

local function socket_path()
  local basename = "nvim-" .. socket_key .. ".sock"
  local candidates = {}
  local runtime = vim.env.XDG_RUNTIME_DIR
  if
    type(runtime) == "string"
    and runtime:match("^/")
    and not runtime:find("\n", 1, true)
    and vim.fn.isdirectory(runtime) == 1
  then
    candidates[#candidates + 1] = runtime .. "/termnav"
  end
  local temporary = vim.env.TMPDIR
  if
    type(temporary) == "string"
    and temporary:match("^/")
    and not temporary:find("\n", 1, true)
    and vim.fn.isdirectory(temporary) == 1
  then
    candidates[#candidates + 1] = temporary .. "/termnav-" .. tostring(user_id())
  end
  candidates[#candidates + 1] = "/tmp/termnav-" .. tostring(user_id())

  -- macOS has the smaller common Unix-domain socket limit. Staying below 100
  -- bytes leaves room for the terminating NUL on every supported platform.
  for _, directory in ipairs(candidates) do
    local path = directory .. "/" .. basename
    if #path <= 100 then
      ensure_private_runtime_dir(directory)
      return path
    end
  end
  error("cannot create a Neovim socket within the platform path limit")
end

local function atomic_write(path, value)
  publish_counter = publish_counter + 1
  local temp = path .. ".tmp." .. owner_key .. "." .. tostring(publish_counter)
  local fd, open_error = uv.fs_open(temp, "wx", 384)
  if not fd then
    error("cannot create registry record: " .. tostring(open_error))
  end

  local function abort(message)
    pcall(uv.fs_close, fd)
    pcall(uv.fs_unlink, temp)
    error(message)
  end

  local content = value .. "\n"
  local written, write_error = uv.fs_write(fd, content, -1)
  if written ~= #content then
    abort("cannot write registry record: " .. tostring(write_error or "short write"))
  end
  local synced, sync_error = uv.fs_fsync(fd)
  if not synced then
    abort("cannot sync registry record: " .. tostring(sync_error))
  end
  local closed, close_error = uv.fs_close(fd)
  fd = nil
  if not closed then
    pcall(uv.fs_unlink, temp)
    error("cannot close registry record: " .. tostring(close_error))
  end
  local renamed, rename_error = uv.fs_rename(temp, path)
  if not renamed then
    pcall(uv.fs_unlink, temp)
    error("cannot publish registry record: " .. tostring(rename_error))
  end
end

local function publication_record(address)
  if type(address) ~= "string" or address == "" or address:find("\n", 1, true) then
    error("cannot publish an invalid Neovim socket address")
  end
  local sequence = string.format("%020.0f", uv.hrtime())
  return table.concat({ "v2", sequence, owner_key, address }, "\n")
end

function M.server()
  -- Keep one advertised address even if this instance had to start its own
  -- RPC socket; v:servername is not guaranteed to change after serverstart().
  if server_address and server_address ~= "" then
    return server_address
  end

  if vim.v.servername ~= "" and not vim.v.servername:find("\n", 1, true) then
    server_address = vim.v.servername
  else
    if not state_dir then
      return ""
    end
    ensure_private_dir(state_dir)
    local socket = socket_path()
    vim.fn.delete(socket)
    local started = vim.fn.serverstart(socket)
    if type(started) ~= "string" or started == "" then
      error("cannot start Neovim RPC server at " .. socket)
    end
    server_address = started
    owned_socket = socket
  end

  return server_address
end

local function stop_owned_server()
  if not owned_socket then
    return
  end

  if server_address and server_address ~= "" then
    pcall(vim.fn.serverstop, server_address)
  end
  pcall(vim.fn.delete, owned_socket)
  owned_socket = nil
  server_address = nil
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

  local registry_dir = state_dir .. "/registry"
  local panes_dir = registry_dir .. "/panes"
  local current_dir = registry_dir .. "/current"
  local owners_dir = registry_dir .. "/owners"
  local pane_dir = panes_dir .. "/" .. pane_key()
  local pane_owners_dir = pane_dir .. "/owners"
  local current_owners_dir = current_dir .. "/owners"
  local pane_record = pane_owners_dir .. "/" .. owner_key
  local current_record = current_owners_dir .. "/" .. owner_key
  local pane_latest = pane_dir .. "/latest"
  local current_latest = current_dir .. "/latest"
  local owner_record = owners_dir .. "/" .. owner_key

  ensure_private_dir(state_dir)
  ensure_private_dir(registry_dir)
  ensure_private_dir(panes_dir)
  ensure_private_dir(current_dir)
  ensure_private_dir(owners_dir)
  ensure_private_dir(pane_dir)
  ensure_private_dir(pane_owners_dir)
  ensure_private_dir(current_owners_dir)

  local function publish()
    local address = M.server()
    local record = publication_record(address)
    atomic_write(pane_record, record)
    atomic_write(current_record, record)
    -- Each complete latest record is its scope's linearization point. Scope
    -- readers never need to join a pointer with another mutable file.
    atomic_write(pane_latest, record)
    atomic_write(current_latest, record)
  end

  local function cleanup()
    vim.fn.delete(owner_record)
    vim.fn.delete(pane_record)
    vim.fn.delete(current_record)
    stop_owned_server()
  end

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
    callback = cleanup,
  })

  local published, publish_error = pcall(publish)
  if published then
    published, publish_error = pcall(atomic_write, owner_record, "v2")
  end
  if not published then
    pcall(vim.api.nvim_del_augroup_by_id, group)
    cleanup()
    error(publish_error)
  end

  return true
end

return M
