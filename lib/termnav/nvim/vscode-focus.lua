-- selene: allow(undefined_variable)
-- Publish leased Neovim focus ownership to the VS Code window displaying it.

local vim = vim
local uv = vim.uv or vim.loop
local M = {}

local function source_dir()
  local info = debug.getinfo(1, "S")
  return (info.source:gsub("^@", "")):match("^(.*)/[^/]+$") or "."
end

local function default_command()
  return vim.fn.resolve(vim.fn.fnamemodify(source_dir() .. "/../../../bin/vscode-nvim-focus", ":p"))
end

local function default_observed()
  return math.floor(uv.hrtime() / 1000000)
end

local function default_source()
  return string.format("nvim-%d-%d", vim.fn.getpid(), default_observed())
end

function M.new(options)
  options = options or {}
  local ctx = {
    command = options.command or default_command(),
    interval_ms = options.interval_ms or 1000,
    observed = options.observed or default_observed,
    source = options.source or default_source(),
    cycle = 0,
    sequence = 0,
    focused = false,
    pending = false,
    timer = nil,
  }

  local function stop_timer()
    if ctx.timer then
      ctx.timer:stop()
      ctx.timer:close()
      ctx.timer = nil
    end
  end

  local function schedule_claim()
    stop_timer()
    if not ctx.focused then
      return
    end
    local timer = uv.new_timer()
    ctx.timer = timer
    timer:start(ctx.interval_ms, 0, function()
      timer:stop()
      timer:close()
      if ctx.timer == timer then
        ctx.timer = nil
      end
      vim.schedule(function()
        ctx.claim()
      end)
    end)
  end

  local function run(operation, callback)
    ctx.sequence = ctx.sequence + 1
    local command = {
      ctx.command,
      operation,
      ctx.source,
      tostring(ctx.cycle),
      tostring(ctx.sequence),
      tostring(ctx.observed()),
    }
    if options.run then
      options.run(command, callback)
      return
    end
    if vim.system then
      vim.system(command, { text = true }, function(result)
        vim.schedule(function()
          callback(result.code)
        end)
      end)
      return
    end
    vim.fn.jobstart(command, {
      on_exit = function(_, code)
        vim.schedule(function()
          callback(code)
        end)
      end,
    })
  end

  function ctx.claim()
    if not ctx.focused or ctx.pending then
      return
    end
    local claim_cycle = ctx.cycle
    ctx.pending = true
    run("claim", function(code)
      ctx.pending = false
      if not ctx.focused then
        return
      end
      if ctx.cycle ~= claim_cycle then
        ctx.claim()
        return
      end
      if code == 0 or code == 1 then
        schedule_claim()
      end
    end)
  end

  function ctx.focus()
    if ctx.focused then
      return
    end
    ctx.focused = true
    ctx.cycle = ctx.cycle + 1
    ctx.claim()
  end

  function ctx.blur()
    if not ctx.focused then
      return
    end
    ctx.focused = false
    stop_timer()
    run("release", function() end)
  end

  function ctx.dispose()
    ctx.blur()
    stop_timer()
  end

  return ctx
end

return M
