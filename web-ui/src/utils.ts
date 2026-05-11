export interface AppError {
  title: string;
  message: string;
  solution: string;
}

export function parseError(e: unknown): AppError {
  let message = ''
  const errorObj = e as any | null;
  const networkLike = errorObj?.message === 'Network Error'
  const isTimeout = errorObj?.code === 'ECONNABORTED'
  const errorCode = String(errorObj?.response?.data?.code || '')

  if (networkLike) {
    message = '无法连接后端服务（/backend）。'
  } else if (isTimeout) {
    message = '请求超时，可能是 AI 响应时间过长或网络代理导致连接缓慢。'
  } else {
    const response = errorObj?.response as any | null;
    const data = response?.data as string | any | null;
    if (typeof data === 'string') {
      try {
        const parsed = JSON.parse(data)
        message = String(parsed?.message || parsed?.error || data)
      } catch {
        message = data
      }
    } else {
      message = String(data?.message || data?.error || errorObj?.message || '未知错误')
    }
  }

  const msgLower = message.toLowerCase()

  if (errorCode === 'ERR_CANCELED' || msgLower.includes('err_canceled') || msgLower.includes('query canceled') || msgLower.includes('request canceled')) {
    return {
      title: 'Request canceled (Canceled)',
      message: message || 'Query canceled',
      solution: 'The running query was canceled. Run it again if needed.',
    }
  }

  if (networkLike) {
    return {
      title: '服务未启动 (Network Error)',
      message,
      solution: '请确保本地的 Rust 后端服务已启动并在运行，且没有任何端口占用或防火墙拦截。',
    }
  }
  if (isTimeout) {
    return {
      title: '请求超时 (Timeout)',
      message,
      solution: '后端处理或 AI 响应超过 120 秒，请检查您的网络代理配置是否通畅。',
    }
  }
  if (msgLower.includes('invalid session token') || msgLower.includes('401') || msgLower.includes('unauthorized') || msgLower.includes('authentication failed') || msgLower.includes('incorrect api key')) {
    return {
      title: 'AI 鉴权失败 (Auth Error)',
      message,
      solution: '当前配置的 AI Token 无效、已过期或格式错误。请在配置页重新填写对应服务商的有效 API Key。',
    }
  }
  if (msgLower.includes('access denied') || msgLower.includes('unknown database') || msgLower.includes('connection refused') || msgLower.includes('communications link failure') || msgLower.includes('failed to connect')) {
    return {
      title: '数据库连接失败 (DB Connection)',
      message,
      solution: '请检查您的数据库地址、端口是否开放，以及账号密码、IP白名单权限是否配置正确。',
    }
  }
  if (msgLower.includes('error in your sql syntax') || msgLower.includes("table doesn't exist") || msgLower.includes('unknown column')) {
    return {
      title: 'SQL 语法/结构错误 (DB Execution)',
      message,
      solution: 'AI 生成的 SQL 可能由于缺乏上下文导致表名/字段名错误。请在侧边栏上传真实的 .sql 结构文件，或手动在编辑器中修改报错的字段。',
    }
  }
  if (msgLower.includes('429') || msgLower.includes('too many requests') || msgLower.includes('timeout') || msgLower.includes('rate limit')) {
    return {
      title: 'AI 请求受限 (Rate Limit / Timeout)',
      message,
      solution: '请求过于频繁或被服务商风控。建议在配置中选择 Pool (Token池) 模式填入多个 Key 以自动轮询重试。',
    }
  }
  if (msgLower.includes('failed to parse sql')) {
    return {
      title: '离线 SQL 解析失败 (Parse Error)',
      message,
      solution: '文件包含了不受支持的专有语法方言，请确保上传标准格式的 MySQL DDL (CREATE TABLE) 语句。',
    }
  }

  if (msgLower.includes('dangerous_sql')) {
    return {
      title: '高危操作警告 (Dangerous SQL)',
      message,
      solution: '系统检测到该 SQL 可能对数据造成不可逆的修改或删除，已拦截执行请求。若确需执行，请在弹窗中确认。',
    }
  }

  return {
    title: '系统错误 (System Error)',
    message,
    solution: '请检查您的输入内容或查看终端运行日志获取详细信息。',
  }
}

export function formatErr(e: unknown): string {
  const err = parseError(e);
  return `${err.title}：${err.message}\n[解决方案]：${err.solution}`;
}

export function redactSensitiveText(input: string): string {
  return input
    .replace(/Bearer\s+[A-Za-z0-9._-]+/g, 'Bearer ******')
    .replace(/(api_key|apiKey)\s*[:=]\s*("?)[^"\s,)};]+(\2)/g, '$1: ******')
    .replace(/password\s*[:=]\s*("?)[^"\s,)};]+(\1)/g, 'password: ******')
    .replace(/:\/\/([^:/?#]+):([^@/]+)@/g, '://$1:******@')
}

export function sanitizeForLog(e: unknown): string {
  const anyErr = e as any
  const base =
    typeof e === 'string'
      ? e
      : typeof anyErr?.message === 'string'
        ? anyErr.message
        : (() => {
            try {
              return JSON.stringify(e)
            } catch {
              return String(e)
            }
          })()
  return redactSensitiveText(base)
}
