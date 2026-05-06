import { getLocale } from '../i18n'

export const ErrorDictionary: Record<'zh' | 'en', Record<string, string>> = {
  zh: {
    ERR_DB_CONNECTION: '无法连接到数据库，请检查您的网络和账号信息。',
    ERR_SQL_SYNTAX: 'SQL 语法错误，请检查您的查询语句。',
    ERR_AI_TIMEOUT: 'AI 助手响应超时或发生错误，请稍后重试。',
    ERR_AI_RATE_LIMITED: 'AI 服务触发限流，请稍后重试或降低请求频率。',
    ERR_AI_AUTH: 'AI 鉴权失败：请检查 API Key/Token 是否有效。',
    ERR_AI_FORBIDDEN: 'AI 访问被拒绝（403）：请检查账号权限、额度或服务商风控。',
    ERR_AI_MODEL_NOT_FOUND: 'AI 模型不存在（404）：请切换模型或检查中转站是否支持该模型。',
    ERR_AI_PROXY: '代理连接失败：请检查代理地址/鉴权，或临时关闭代理后重试。',
    ERR_EXTERNAL_UNAVAILABLE: '外部服务不可用或网络连接失败，请检查网络/代理设置。',
    ERR_TIMEOUT: '请求超时：请检查网络/代理设置，或稍后重试。',
    ERR_NOT_FOUND: '请求的资源不存在。',
    ERR_UNAUTHORIZED: '未授权访问，请检查您的凭证或权限。',
    ERR_BAD_REQUEST: '请求参数错误，请检查您的输入。',
    ERR_PARSE: '数据解析失败，请检查输入格式。',
    ERR_INTERNAL: '服务器内部错误，请联系管理员或稍后重试。',
    ERR_FORBIDDEN: '访问被拒绝，请检查权限。',
    ERR_PAYLOAD_TOO_LARGE: '上传内容过大，请调整文件大小后重试。',
    ERR_RESOURCE_LIMIT: '资源限制触发，请降低并发或稍后重试。',
    ERR_CONCURRENCY_LIMIT: '请求过多，请稍后重试。',
    ERR_CANCELED: '请求已取消。',
  },
  en: {
    ERR_DB_CONNECTION: 'Unable to connect to the database. Please check network and credentials.',
    ERR_SQL_SYNTAX: 'SQL syntax error. Please check your query.',
    ERR_AI_TIMEOUT: 'AI request timed out or failed. Please try again later.',
    ERR_AI_RATE_LIMITED: 'AI service is rate limited. Please try again later.',
    ERR_AI_AUTH: 'AI authentication failed. Please check API key/token.',
    ERR_AI_FORBIDDEN: 'AI request forbidden (403). Please check permissions/quota.',
    ERR_AI_MODEL_NOT_FOUND: 'AI model not found (404). Please switch model or check relay support.',
    ERR_AI_PROXY: 'Proxy connection failed. Please check proxy settings.',
    ERR_EXTERNAL_UNAVAILABLE: 'External service unavailable or network error. Please check your network/proxy.',
    ERR_TIMEOUT: 'Request timed out. Please try again later.',
    ERR_NOT_FOUND: 'Requested resource not found.',
    ERR_UNAUTHORIZED: 'Unauthorized. Please check your credentials.',
    ERR_BAD_REQUEST: 'Bad request. Please check your input.',
    ERR_PARSE: 'Failed to parse data. Please check input format.',
    ERR_INTERNAL: 'Internal server error. Please try again later.',
    ERR_FORBIDDEN: 'Access forbidden.',
    ERR_PAYLOAD_TOO_LARGE: 'Payload too large. Please reduce file size and retry.',
    ERR_RESOURCE_LIMIT: 'Resource limit exceeded. Please reduce concurrency or retry later.',
    ERR_CONCURRENCY_LIMIT: 'Too many requests. Please retry later.',
    ERR_CANCELED: 'Request canceled.',
  },
}

export const getErrorMessage = (code: string, defaultMessage: string = 'Unknown error'): string => {
  const locale = getLocale()
  return ErrorDictionary[locale]?.[code] || defaultMessage
}
