-- For development only. A token whose address has no account row (stored after the up
-- migration, or whose account was deleted) is left with an empty account_id.
ALTER TABLE public.token
    ADD COLUMN account_id int8 REFERENCES public.account (id) ON DELETE CASCADE;

UPDATE public.token t
SET account_id = a.id
FROM public.account a
WHERE a.address = t.address;

ALTER TABLE public.token
    ADD CONSTRAINT unique_account_id_constraint UNIQUE (account_id);
CREATE INDEX idx_token_account_id ON public.token (account_id);

ALTER TABLE public.token
    DROP CONSTRAINT token_address_key,
    ALTER COLUMN address DROP NOT NULL;
