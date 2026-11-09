export const cache = 'yes';
export function transform(request) {
  return request.input.text.toUpperCase();
}
