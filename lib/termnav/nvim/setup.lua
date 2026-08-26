-- selene: allow(undefined_variable)
local vim = vim

local M = {}

-- Write side of termnav's private WezTerm user-var protocol. The wezterm reader
-- in lib/termnav/wezterm/link-routes.lua declares the same names; the two run in
-- different Lua hosts (nvim vs wezterm) and cannot share a module reliably
-- (wezterm has no dependable debug source path), so keep the two tables in sync.
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
  local path = source_dir() .. "/nvim-tmux-open.lua"
  local canonical = vim.fn.resolve(vim.fn.fnamemodify(path, ":p"))
  -- Reuse one process-scoped owner across config reloads and source aliases.
  local cache_key = "termnav.nvim.default_opener:" .. canonical
  local opener = package.loaded[cache_key]
  if opener == nil then
    opener = dofile(path)
    package.loaded[cache_key] = opener
  end
  return opener
end

local function default_wezterm_vars()
  return load_sibling("wezterm-vars.lua")
end

local function default_vscode_focus()
  return load_sibling("vscode-focus.lua").new()
end

local function default_navigation()
  return load_sibling("navigation.lua").new()
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
    navigation = options.navigation or default_navigation(),
    wezterm_vars = options.wezterm_vars or default_wezterm_vars(),
    vscode_focus = options.vscode_focus or default_vscode_focus(),
    publish_delay_ms = options.publish_delay_ms or 100,
    published_vars = {},
    remote_link_host = nil,
    focused = false,
    focus_generation = 0,
    publish_events = option_list(
      options,
      "publish_events",
      { "BufEnter", "DirChanged", "WinEnter" }
    ),
    refresh_events = option_list(options, "refresh_events", { "FocusGained", "VimResume" }),
    clear_events = option_list(options, "clear_events", { "VimLeave", "FocusLost", "VimSuspend" }),
  }

  function ctx.set_user_var(name, value, batch)
    value = tostring(value or "")
    -- WezTerm user vars are terminal OSC writes. Hot editor events should refresh
    -- link context freely, but unchanged values must not keep writing to the tty.
    if ctx.published_vars[name] == value then
      return
    end

    -- Publishing pane metadata is best-effort: startup/focus events will retry if
    -- tmux tty discovery or WezTerm passthrough is briefly unavailable.
    if ctx.wezterm_vars.set(name, value, batch) then
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

  function ctx.publish_metadata(batch)
    if not ctx.focused then
      return
    end
    local remote_host = ctx.nvim_remote_link_host()
    local cwd = vim.fn.getcwd()
    batch = batch or {}

    ctx.set_user_var(user_vars.open_socket, ctx.opener.server(), batch)
    -- Terminal text links are produced relative to the process cwd that printed
    -- them, not necessarily the active workspace root or WezTerm's stale pane cwd.
    ctx.set_user_var(user_vars.link_cwd, cwd, batch)
    ctx.set_user_var(user_vars.remote_link_host, remote_host, batch)
    ctx.set_user_var(user_vars.remote_cwd, remote_host ~= "" and cwd or "", batch)
    -- Direct remote-pane routing sends a tmux command through the visible
    -- terminal; advertise it only when the remote nvim is actually inside tmux.
    ctx.set_user_var(
      user_vars.remote_tmux,
      remote_host ~= "" and vim.env.TMUX and "true" or "",
      batch
    )
  end

  function ctx.claim_focus(batch)
    if not ctx.focused then
      ctx.focused = true
      ctx.focus_generation = ctx.focus_generation + 1
      ctx.vscode_focus.focus()
    end
    ctx.set_user_var(user_vars.is_nvim, "true", batch or {})
  end

  function ctx.publish()
    local batch = {}
    ctx.claim_focus(batch)
    ctx.publish_metadata(batch)
  end

  function ctx.refresh()
    ctx.remote_link_host = nil
    ctx.publish()
  end

  function ctx.clear()
    local batch = {}
    ctx.focused = false
    ctx.focus_generation = ctx.focus_generation + 1
    ctx.vscode_focus.blur()
    ctx.set_user_var(user_vars.is_nvim, "false", batch)
    ctx.set_user_var(user_vars.open_socket, "", batch)
    ctx.set_user_var(user_vars.link_cwd, "", batch)
    ctx.set_user_var(user_vars.remote_link_host, "", batch)
    ctx.set_user_var(user_vars.remote_cwd, "", batch)
    ctx.set_user_var(user_vars.remote_tmux, "", batch)
    -- The next focus may belong to a newly attached terminal. Republish every
    -- value even when this clear could not determine the passthrough depth.
    ctx.published_vars = {}
  end

  function ctx.setup()
    local group = vim.api.nvim_create_augroup(ctx.group_name, { clear = true })

    ctx.navigation.setup()
    ctx.opener.setup()
    ctx.claim_focus()

    -- Lazy-loading configs may call setup after VimEnter. Shell preexec normally
    -- marks the pane before nvim starts; this deferred publish covers non-shell
    -- launches and refreshes socket/cwd metadata once nvim has settled.
    if ctx.publish_delay_ms and ctx.publish_delay_ms >= 0 then
      local generation = ctx.focus_generation
      vim.defer_fn(function()
        if ctx.focused and ctx.focus_generation == generation then
          ctx.publish_metadata()
        end
      end, ctx.publish_delay_ms)
    end

    vim.api.nvim_create_autocmd(ctx.refresh_events, {
      group = group,
      callback = ctx.refresh,
    })

    vim.api.nvim_create_autocmd(ctx.publish_events, {
      group = group,
      callback = ctx.publish_metadata,
    })

    -- Multiple tmux panes share one WezTerm pane. Clear ownership on focus loss
    -- so WezTerm key policy does not keep treating the pane as active nvim.
    vim.api.nvim_create_autocmd(ctx.clear_events, {
      group = group,
      callback = ctx.clear,
    })

    vim.api.nvim_create_autocmd("VimLeavePre", {
      group = group,
      callback = function()
        ctx.vscode_focus.dispose()
      end,
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
