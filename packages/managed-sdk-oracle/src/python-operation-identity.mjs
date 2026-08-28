export function canonicalPythonOperationID(id) {
  return id
    .split('.')
    .map((segment) => segment.replace(/_([a-z])/gu, (_, letter) => letter.toUpperCase()))
    .join('.')
    .replace(/createEnrollmentUrl$/u, 'createEnrollmentURL')
    .replace(/mcpOauthValidate$/u, 'mcpOAuthValidate');
}
