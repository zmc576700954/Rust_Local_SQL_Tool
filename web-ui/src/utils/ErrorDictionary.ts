export const ErrorDictionary: Record<string, string> = {
  ERR_DB_CONNECTION: "无法连接到数据库，请检查您的网络和账号信息。",
  ERR_SQL_SYNTAX: "SQL 语法错误，请检查您的查询语句。",
  ERR_AI_TIMEOUT: "AI 助手响应超时或发生错误，请稍后重试。",
  ERR_AI_RATE_LIMITED: "AI 服务触发限流，请稍后重试或降低请求频率。",
  ERR_AI_AUTH: "AI 鉴权失败：请检查 API Key/Token 是否有效。",
  ERR_AI_FORBIDDEN: "AI 访问被拒绝（403）：请检查账号权限、额度或服务商风控。",
  ERR_AI_MODEL_NOT_FOUND: "AI 模型不存在（404）：请切换模型或检查中转站是否支持该模型。",
  ERR_AI_PROXY: "代理连接失败：请检查代理地址/鉴权，或临时关闭代理后重试。",
  ERR_EXTERNAL_UNAVAILABLE: "外部服务不可用或网络连接失败，请检查网络/代理设置。",
  ERR_TIMEOUT: "请求超时：请检查网络/代理设置，或稍后重试。",
  ERR_NOT_FOUND: "请求的资源不存在。",
  ERR_UNAUTHORIZED: "未授权访问，请检查您的凭证或权限。",
  ERR_BAD_REQUEST: "请求参数错误，请检查您的输入。",
  ERR_PARSE: "数据解析失败，请检查输入格式。",
  ERR_INTERNAL: "服务器内部错误，请联系管理员或稍后重试。",
};

export const getErrorMessage = (code: string, defaultMessage: string = "发生未知错误"): string => {
  return ErrorDictionary[code] || defaultMessage;
};
