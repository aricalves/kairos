-- Reverse of KAIROS-T-0399: removes the breakers of the code index
-- builder. The builder then builds each repository again.
DROP TABLE IF EXISTS code_index_breakers;
