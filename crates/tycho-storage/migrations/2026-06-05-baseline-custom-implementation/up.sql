UPDATE protocol_type
SET implementation = 'custom'
WHERE name = 'baseline'
  AND implementation = 'vm';
