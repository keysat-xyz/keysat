// Action: set the operator display name shown on the service homepage.

import { sdk } from '../sdk'

const input = sdk.InputSpec.of({
  operator_name: {
    type: 'text',
    name: 'Operator name',
    description:
      'Displayed on the service homepage so buyers know whose Keysat ' +
      'instance they are interacting with. E.g., your name or business name.',
    required: true,
    default: null,
  },
})

export const setOperatorName = sdk.Action.withInput(
  'setOperatorName',
  async ({ effects }) => ({
    name: 'Set operator name',
    description: 'Edit the operator name shown publicly.',
    warning: null,
    allowedStatuses: 'any',
    group: 'General',
    visibility: 'enabled',
  }),
  input,
  async ({ effects, input: formInput }) => {
    const current = await sdk.store.getOwn(effects, sdk.StorePath).const()
    await sdk.store.setOwn(effects, sdk.StorePath, {
      ...current,
      operator_name: formInput.operator_name,
    })
    return { message: `Operator name set to ${formInput.operator_name}. Restart the service to apply.` }
  },
)
