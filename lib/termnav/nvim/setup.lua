-- selene: allow(undefined_variable)
local vim = vim

local M = {}

local user_vars = {
  is_nvim = "IS_NVIM",
  open_socket = "NVIM_OPEN_SOCKET",
  link_cwd = "NVIM_LINK_CWD",
  remote_link_host = "NVIM_REMOTE_LINK_HOST",
  remote_cwd = "NVIM_REMOTE_CWD",
  remote_tmux = "NVIM_REMOTE_TMUX",
}

local env_vars = {
  remote_link_host = "TERMNAV_REMOTE_LINK_HOST",
}

local function source_dir()
  if type(debug) == "table" and type(debug.getinfo) == "function" then
    local info = debug.getinfo(1, "S")
    local source = (info and info.source or ""):gsub("^@", ""):gsub("\\", "/")
    return source:match("^(.*)/[^/]+$")
  end

  return "."
end

local function load_sibling(name)
  return dofile(source_dir() .. "/" .. name)
end

local function default_opener()
  return load_sibling("nvim-tmux-open.lua")
end

local function default_wezterm_vars()
  return load_sibling("wezterm-vars.lua")
end

local function option_list(options, name, default)
  local value = options[name]
  if value == nil then
    return default
  end
  return value
end

function M.new(options)
  options = options or {}

  local ctx = {
    group_name = options.group_name or "termnav_nvim",
    opener = options.opener or default_opener(),
    wezterm_vars = options.wezterm_vars or default_wezterm_vars(),
    publish_delay_ms = options.publish_delay_ms or 100,
    published_vars = {},
    remote_link_host = nil,
    publish_events = option_list(
      options,
      "publish_events",
      { "BufEnter", "DirChanged", "WinEnter" }
    ),
    refresh_events = option_list(options, "refresh_events", { "FocusGained", "VimResume" }),
    clear_events = option_list(options, "clear_events", { "VimLeave", "FocusLost", "VimSuspend" }),
  }

  function ctx.set_user_var(name, value)
    value = tostring(value or "")
    -- WezTerm user vars are terminal OSC writes. Hot editor events should refresh
    -- link context freely, but unchanged values must not keep writing to the tty.
    if ctx.published_vars[name] == value then
      return
    end

    -- Publishing pane metadata is best-effort: startup/focus events will retry if
    -- tmux tty discovery or WezTerm passthrough is briefly unavailable.
    if ctx.wezterm_vars.set(name, value) then
      ctx.published_vars[name] = value
    end
  end

  function ctx.tmux_remote_link_host()
    if not vim.env.TMUX then
      return ""
    end

    -- Managed or persistent remote transports can seed tmux's global env before
    -- nvim starts. That gives already-running shells and editors the same host
    -- identity used by terminal hyperlink generators.
    local output = vim.fn.system({ "tmux", "show-environment", "-g", env_vars.remote_link_host })
    if vim.v.shell_error ~= 0 then
      return ""
    end

    return output:match("^" .. env_vars.remote_link_host .. "=(.-)%s*$") or ""
  end

  function ctx.nvim_remote_link_host()
    if ctx.remote_link_host ~= nil then
      return ctx.remote_link_host
    end

    ctx.remote_link_host = vim.env[env_vars.remote_link_host] or ""
    if ctx.remote_link_host == "" then
      ctx.remote_link_host = ctx.tmux_remote_link_host()
    end
    if ctx.remote_link_host == "" and vim.env.SSH_CONNECTION and vim.env.TMUX then
      ctx.remote_link_host = vim.fn.system({ "hostname", "-s" }):gsub("%s+$", "")
      if ctx.remote_link_host == "" then
        ctx.remote_link_host = vim.fn.system({ "hostname" }):gsub("%s+$", "")
      end
    end

    return ctx.remote_link_host
  end

  function ctx.publish()
    local remote_host = ctx.nvim_remote_link_host()
    local cwd = vim.fn.getcwd()

    ctx.set_user_var(user_vars.is_nvim, "true")
    ctx.set_user_var(user_vars.open_socket, ctx.opener.server())
    -- Terminal text links are produced relative to the process cwd that printed
    -- them, not necessarily the active workspace root or WezTerm's stale pane cwd.
    ctx.set_user_var(user_vars.link_cwd, cwd)
    ctx.set_user_var(user_vars.remote_link_host, remote_host)
    ctx.set_user_var(user_vars.remote_cwd, remote_host ~= "" and cwd or "")
    -- Direct remote-pane routing sends a tmux command through the visible
    -- terminal; advertise it only when the remote nvim is actually inside tmux.
    ctx.set_user_var(user_vars.remote_tmux, remote_host ~= "" and vim.env.TMUX and "true" or "")
  end

  function ctx.refresh()
    ctx.remote_link_host = nil
    ctx.publish()
  end

  function ctx.clear()
    ctx.set_user_var(user_vars.is_nvim, "false")
    ctx.set_user_var(user_vars.open_socket, "")
    ctx.set_user_var(user_vars.link_cwd, "")
    ctx.set_user_var(user_vars.remote_link_host, "")
    ctx.set_user_var(user_vars.remote_cwd, "")
    ctx.set_user_var(user_vars.remote_tmux, "")
  end

  function ctx.setup()
    local group = vim.api.nvim_create_augroup(ctx.group_name, { clear = true })

    ctx.opener.setup()

    -- Lazy-loading configs may call setup after VimEnter. Shell preexec normally
    -- marks the pane before nvim starts; this deferred publish covers non-shell
    -- launches and refreshes socket/cwd metadata once nvim has settled.
    if ctx.publish_delay_ms and ctx.publish_delay_ms >= 0 then
      vim.defer_fn(ctx.publish, ctx.publish_delay_ms)
    end

    vim.api.nvim_create_autocmd(ctx.refresh_events, {
      group = group,
      callback = ctx.refresh,
    })

    vim.api.nvim_create_autocmd(ctx.publish_events, {
      group = group,
      callback = ctx.publish,
    })

    -- Multiple tmux panes share one WezTerm pane. Clear ownership on focus loss
    -- so WezTerm key policy does not keep treating the pane as active nvim.
    vim.api.nvim_create_autocmd(ctx.clear_events, {
      group = group,
      callback = ctx.clear,
    })
  end

  return ctx
end

function M.setup(options)
  local ctx = M.new(options)
  ctx.setup()
  return ctx
end

return M
