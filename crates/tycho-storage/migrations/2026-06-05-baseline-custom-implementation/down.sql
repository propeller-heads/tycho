UPDATE protocol_type
SET implementation = 'vm'
WHERE name = 'baseline'
  AND implementation = 'custom';
